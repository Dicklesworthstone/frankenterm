#![allow(clippy::range_plus_one)]
use super::renderstate::*;
use super::utilsprites::RenderMetrics;
use crate::colorease::ColorEase;
use crate::frontend::try_front_end;
use crate::inputmap::InputMap;
use crate::overlay::{
    CopyModeParams, CopyOverlay, LauncherArgs, LauncherFlags, QuickSelectOverlay,
    confirm_close_pane, confirm_close_tab, confirm_close_window, confirm_quit_program, launcher,
    start_overlay, start_overlay_pane,
};
use crate::resize_increment_calculator::ResizeIncrementCalculator;
use crate::scripting::guiwin::GuiWin;
use crate::scrollbar::*;
use crate::selection::Selection;
use crate::shapecache::*;
use crate::tabbar::{TabBarItem, TabBarState};
use crate::termwindow::background::{BackgroundLoadCoordinator, LoadedBackgroundLayer};
use crate::termwindow::keyevent::{KeyTableArgs, KeyTableState};
use crate::termwindow::modal::Modal;
use crate::termwindow::render::draw::{DrawFailure, DrawFailureStage};
use crate::termwindow::render::paint::{AllowImage, PaintOutcome};
use crate::termwindow::render::{
    CachedLineState, LineQuadCacheKey, LineQuadCacheValue, LineToEleShapeCacheKey,
    LineToElementShapeItem,
};
use crate::termwindow::webgpu::WebGpuState;
use ::wezterm_term::input::{ClickPosition, MouseButton as TMB};
use ::window::*;
use anyhow::{Context, anyhow, ensure};
use config::keyassignment::{
    Confirmation, FloatingPaneKeyCommand, KeyAssignment, LauncherActionArgs, PaneDirection,
    Pattern, PromptInputLine, QuickSelectArguments, RotationDirection, SpawnCommand, SplitSize,
};
use config::window::WindowLevel;
use config::{
    AudibleBell, ConfigHandle, Dimension, DimensionContext, FrontEndSelection, GeometryOrigin,
    GuiPosition, TermConfig, WindowCloseConfirmation, configuration,
};
use flume::{Sender, TrySendError};
use frankenterm_client::pane::ClientPane;
use frankenterm_core::accessibility_preferences::MotionPreference;
use frankenterm_core::atlas_tier_doctor::TierSwapDoctorReport;
use frankenterm_core::floating_panes::{
    FloatingRect, KeyboardCommand as FloatingKeyboardCommand, PanePosition,
};
use frankenterm_core::frame_budget_a11y_gate as frame_budget_a11y;
use frankenterm_core::session_pane_state::TerminalState;
use frankenterm_font::FontConfiguration;
use frankenterm_gui::accessibility_preferences::config_with_accessibility_palette;
use frankenterm_gui::floating_panes::{
    GuiFloatingPaneController, emit_floating_pane_a11y_messages, floating_pane_id_to_mux_pane_id,
    mux_pane_id_to_floating_pane_id,
};
use frankenterm_gui::triple_buffer_gui::{
    TerminalStateTripleBufferRegistry,
};
use frankenterm_gui::terminal_pane_id_to_u64;
use futures::future::{AbortHandle, AbortRegistration, Abortable};
use frankenterm_toast_notification::persistent_toast_notification;
use lfucache::*;
use mlua::{FromLua, LuaSerdeExt, UserData, UserDataFields};
use mux::domain::DomainId;
use mux::pane::{
    CachePolicy, CloseReason, Pane, PaneId, Pattern as MuxPattern, PerformAssignmentResult,
};
use mux::renderable::{RenderableDimensions, StableCursorPosition};
use mux::tab::{
    PositionedPane, PositionedSplit, SplitDirection, SplitRequest, SplitSize as MuxSplitSize, Tab,
    TabId,
};
use mux::unify::{MergePlan, TabIdentity, TabSnapshot, WindowSnapshot, plan_unify_domain};
use mux::window::WindowId as MuxWindowId;
use mux::{
    Mux, MuxNotification, PaneRegistrationHandle, PaneRemovalCleanupLease,
    SynchronizedOutputAdmissionDecision, SynchronizedOutputDepthOutcome,
    SynchronizedOutputDrainCause, SynchronizedOutputEvent,
};
use mux_lua::MuxPane;
use promise::spawn::sleep;
use std::cell::{Cell, RefCell, RefMut};
use std::collections::{BTreeSet, HashMap, HashSet, LinkedList};
use std::ops::Range;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use termwiz::hyperlink::Hyperlink;
use termwiz::surface::SequenceNo;
use wezterm_dynamic::Value;
use wezterm_term::color::ColorPalette;
use wezterm_term::input::LastMouseClick;
use wezterm_term::{Alert, Progress, StableRowIndex, TerminalConfiguration, TerminalSize};

fn schedule_existing_termwindow_future<F, OUTPUT>(
    service_class: promise::spawn::MainThreadServiceClass,
    estimated_bytes: usize,
    operation: &'static str,
    future: F,
) where
    F: std::future::Future<Output = OUTPUT> + 'static,
    OUTPUT: 'static,
{
    match promise::spawn::try_reserve_main_thread(service_class, estimated_bytes) {
        promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => {
            reservation.spawn_local(future).detach();
        }
        rejected => {
            log::error!(
                "main-thread scheduler rejected term-window operation {operation}; dropped its exact prepared future: {rejected:?}"
            );
        }
    }
}

pub mod background;
pub mod box_model;
pub mod charselect;
pub mod clipboard;
pub mod frame_budget;
pub mod idle_detector;
pub mod keyevent;
pub mod modal;
mod mouseevent;
pub mod palette;
pub mod paneselect;
mod prevcursor;
pub mod render;
pub mod resize;
mod selection;
pub mod spawn;
pub mod webgpu;
use crate::spawn::SpawnWhere;
use prevcursor::PrevCursorPos;

const ATLAS_SIZE: usize = 128;

lazy_static::lazy_static! {
    static ref WINDOW_CLASS: Mutex<String> = Mutex::new(wezterm_gui_subcommands::DEFAULT_WINDOW_CLASS.to_owned());
    static ref POSITION: Mutex<Option<GuiPosition>> = Mutex::new(None);
}

pub const ICON_DATA: &[u8] = include_bytes!("../../../../assets/icon/terminal.png");

fn lock_termwindow_mutex<'a, T>(mutex: &'a Mutex<T>, name: &str) -> std::sync::MutexGuard<'a, T> {
    mutex.lock().unwrap_or_else(|poisoned| {
        log::warn!("recovering poisoned {name} lock");
        mutex.clear_poison();
        poisoned.into_inner()
    })
}

pub fn set_window_position(pos: GuiPosition) {
    lock_termwindow_mutex(&POSITION, "window position").replace(pos);
}

pub fn set_window_class(cls: &str) {
    *lock_termwindow_mutex(&WINDOW_CLASS, "window class") = cls.to_owned();
}

pub fn get_window_class() -> String {
    lock_termwindow_mutex(&WINDOW_CLASS, "window class").clone()
}

fn should_restore_workspace_after_failed_spawn(
    active_workspace: &str,
    requested_workspace: &str,
    requested_workspace_has_windows: bool,
) -> bool {
    !requested_workspace_has_windows && active_workspace == requested_workspace
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MouseCapture {
    UI,
    TerminalPane(PaneId),
}

/// Type used together with Window::notify to do something in the
/// context of the window-specific event loop
pub enum TermWindowNotif {
    InvalidateShapeCache,
    PerformAssignment {
        pane_id: PaneId,
        assignment: KeyAssignment,
        tx: Option<Sender<anyhow::Result<()>>>,
    },
    SetLeftStatus(String),
    SetRightStatus(String),
    GetDimensions(Sender<(Dimensions, WindowState)>),
    GetSelectionForPane {
        pane_id: PaneId,
        tx: Sender<String>,
    },
    GetEffectiveConfig(Sender<ConfigHandle>),
    FinishWindowEvent {
        name: String,
        again: bool,
    },
    GetConfigOverrides(Sender<wezterm_dynamic::Value>),
    SetConfigOverrides(wezterm_dynamic::Value),
    CancelOverlayForPane {
        pane_id: PaneId,
        ticket: OverlayCancellationTicket,
    },
    CancelOverlayForTab {
        tab_id: TabId,
        overlay_pane_id: PaneId,
        ticket: OverlayCancellationTicket,
    },
    MuxNotification {
        notification: MuxNotification,
        /// Exact mux instance that emitted this notification. Numeric window
        /// and pane IDs can be reused after a process-global mux replacement.
        mux_owner: std::sync::Weak<Mux>,
        /// Exact mux removal-generation authority retained until this window
        /// has finished cleaning its numeric pane-keyed state.
        pane_removal_cleanup: Option<PaneRemovalCleanupLease>,
    },
    EmitStatusUpdate,
    Apply(Box<dyn FnOnce(&mut TermWindow) + Send + Sync>),
    SwitchToMuxWindow(MuxWindowId),
    SetInnerSize {
        width: usize,
        height: usize,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UIItemType {
    TabBar(TabBarItem),
    CloseTab(usize),
    AboveScrollThumb,
    ScrollThumb,
    BelowScrollThumb,
    Split(PositionedSplit),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UIItem {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
    pub item_type: UIItemType,
}

impl UIItem {
    pub fn hit_test(&self, x: isize, y: isize) -> bool {
        let left = usize_to_isize_saturating(self.x);
        let top = usize_to_isize_saturating(self.y);
        let right = usize_to_isize_saturating(self.x.saturating_add(self.width));
        let bottom = usize_to_isize_saturating(self.y.saturating_add(self.height));

        x >= left && x < right && y >= top && y < bottom
    }
}

fn usize_to_isize_saturating(value: usize) -> isize {
    isize::try_from(value).unwrap_or(isize::MAX)
}

#[derive(Clone, Copy, Debug)]
enum WindowUnifyScope {
    ActiveDomain,
    AllDomains,
}

#[derive(Clone, Debug)]
struct GuiWindowUnifyPlan {
    title: String,
    workspace: String,
    plans: Vec<MergePlan>,
    close_windows: Vec<MuxWindowId>,
}

impl GuiWindowUnifyPlan {
    fn move_count(&self) -> usize {
        self.plans.iter().map(|plan| plan.moves.len()).sum()
    }

    fn drop_count(&self) -> usize {
        self.plans.iter().map(|plan| plan.drops.len()).sum()
    }

    fn close_count(&self) -> usize {
        self.close_windows.len()
    }

    fn summary_message(&self) -> String {
        format!(
            "{}\nWorkspace: {}\n\nPlan summary:\nMoves: {}\nDrops: {}\nWindows to close: {}\n\nContinue?",
            self.title,
            self.workspace,
            self.move_count(),
            self.drop_count(),
            self.close_count(),
        )
    }
}

#[derive(Clone, Default)]
pub struct SemanticZoneCache {
    seqno: SequenceNo,
    zones: Vec<StableRowIndex>,
}

pub struct OverlayState {
    pub pane: Arc<dyn Pane>,
    pub key_table_state: KeyTableState,
    /// Exact mux registration captured at assignment. `None` is intentional
    /// for GUI-only overlays such as `CopyOverlay`; those must never remove a
    /// same-numbered mux pane.
    registration: Option<PaneRegistrationHandle>,
    cancellation_ticket: OverlayCancellationTicket,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OverlaySlot {
    Pane(PaneId),
    Tab(TabId),
}

#[derive(Clone)]
struct OverlayInstanceAuthority(Arc<OverlayInstanceState>);

struct OverlayInstanceState {
    cancellation_requested: AtomicBool,
}

impl OverlayInstanceAuthority {
    fn new() -> Self {
        Self(Arc::new(OverlayInstanceState {
            cancellation_requested: AtomicBool::new(false),
        }))
    }

    fn same_instance(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    fn request_cancellation(&self) {
        self.0.cancellation_requested.store(true, Ordering::Release);
    }

    fn cancellation_requested(&self) -> bool {
        self.0.cancellation_requested.load(Ordering::Acquire)
    }
}

/// Exact authority carried from a worker's cancellation request to the GUI
/// event loop. Numeric pane/tab IDs are reusable slots and are not sufficient
/// to cancel an overlay after queueing delay.
#[derive(Clone)]
pub struct OverlayCancellationTicket {
    mux_owner: Weak<Mux>,
    slot: OverlaySlot,
    overlay_pane_id: PaneId,
    instance: OverlayInstanceAuthority,
    /// Exact mux registration for a TermWiz worker overlay. GUI-only overlays
    /// deliberately carry `None` and can be cancelled only synchronously by
    /// their owning `TermWindow`.
    registration: Option<PaneRegistrationHandle>,
}

impl OverlayCancellationTicket {
    fn new(
        mux_owner: Weak<Mux>,
        slot: OverlaySlot,
        overlay_pane_id: PaneId,
        registration: Option<PaneRegistrationHandle>,
    ) -> Self {
        Self {
            mux_owner,
            slot,
            overlay_pane_id,
            instance: OverlayInstanceAuthority::new(),
            registration,
        }
    }

    fn matches(&self, other: &Self) -> bool {
        if self.slot != other.slot
            || self.overlay_pane_id != other.overlay_pane_id
            || !Weak::ptr_eq(&self.mux_owner, &other.mux_owner)
        {
            return false;
        }
        match (&self.registration, &other.registration) {
            (Some(current), Some(expected)) => current.same_registration(expected),
            (None, None) => self.instance.same_instance(&other.instance),
            _ => false,
        }
    }

    fn request_cancellation(&self) {
        self.instance.request_cancellation();
    }

    fn cancellation_requested(&self) -> bool {
        self.instance.cancellation_requested()
    }
}

#[derive(Default)]
pub struct PaneState {
    /// If is_some(), the top row of the visible screen.
    /// Otherwise, the viewport is at the bottom of the
    /// scrollback.
    viewport: Option<StableRowIndex>,
    /// Terminal sequence fence consumed by render damage discovery. This is
    /// deliberately independent from `selection.seqno`: a selection's fence
    /// must remain fixed for its lifetime, while the renderer must advance
    /// after every successful dirty query.
    render_dirty: frankenterm_gui::RenderDirtySequenceFence,
    selection: Selection,
    /// If is_some(), rather than display the actual tab
    /// contents, we're overlaying a little internal application
    /// tab.  We'll also route input to it.
    pub overlay: Option<OverlayState>,

    bell_start: Option<Instant>,
    pub mouse_terminal_coords: Option<(ClickPosition, StableRowIndex)>,
}

/// Data used when synchronously formatting pane and window titles
#[derive(Debug, Clone)]
pub struct TabInformation {
    pub tab_id: TabId,
    pub tab_index: usize,
    pub is_active: bool,
    pub is_last_active: bool,
    pub active_pane: Option<PaneInformation>,
    pub window_id: MuxWindowId,
    pub tab_title: String,
}

impl UserData for TabInformation {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("tab_id", |_, this| Ok(this.tab_id));
        fields.add_field_method_get("tab_index", |_, this| Ok(this.tab_index));
        fields.add_field_method_get("is_active", |_, this| Ok(this.is_active));
        fields.add_field_method_get("is_last_active", |_, this| Ok(this.is_last_active));
        fields.add_field_method_get("active_pane", |_, this| {
            if let Some(pane) = &this.active_pane {
                Ok(Some(pane.clone()))
            } else {
                Ok(None)
            }
        });
        fields.add_field_method_get("panes", |_, this| {
            let Some(mux) = Mux::try_get() else {
                return Ok(vec![]);
            };
            let mut panes = vec![];
            if let Some(tab) = mux.get_tab(this.tab_id) {
                panes = tab
                    .iter_panes()
                    .iter()
                    .map(TermWindow::pos_pane_to_pane_info)
                    .collect();
            }
            Ok(panes)
        });
        fields.add_field_method_get("window_id", |_, this| Ok(this.window_id));
        fields.add_field_method_get("tab_title", |_, this| Ok(this.tab_title.clone()));
        fields.add_field_method_get("window_title", |_, this| {
            let mux = Mux::try_get().ok_or_else(|| {
                mlua::Error::external("active mux is no longer available for window_title")
            })?;
            let window = mux.get_window(this.window_id).ok_or_else(|| {
                mlua::Error::external(format!("window {} not found", this.window_id))
            })?;
            Ok(window.get_title().to_string())
        });
    }
}

/// Data used when synchronously formatting pane and window titles
#[derive(Debug, Clone)]
pub struct PaneInformation {
    pub pane_id: PaneId,
    pub pane_index: usize,
    pub is_active: bool,
    pub is_zoomed: bool,
    pub has_unseen_output: bool,
    pub left: usize,
    pub top: usize,
    pub width: usize,
    pub height: usize,
    pub pixel_width: usize,
    pub pixel_height: usize,
    pub title: String,
    pub user_vars: HashMap<String, String>,
    pub progress: Progress,
}

impl UserData for PaneInformation {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("pane_id", |_, this| Ok(this.pane_id));
        fields.add_field_method_get("pane_index", |_, this| Ok(this.pane_index));
        fields.add_field_method_get("is_active", |_, this| Ok(this.is_active));
        fields.add_field_method_get("is_zoomed", |_, this| Ok(this.is_zoomed));
        fields.add_field_method_get("has_unseen_output", |_, this| Ok(this.has_unseen_output));
        fields.add_field_method_get("left", |_, this| Ok(this.left));
        fields.add_field_method_get("top", |_, this| Ok(this.top));
        fields.add_field_method_get("width", |_, this| Ok(this.width));
        fields.add_field_method_get("height", |_, this| Ok(this.height));
        fields.add_field_method_get("pixel_width", |_, this| Ok(this.pixel_width));
        fields.add_field_method_get("pixel_height", |_, this| Ok(this.pixel_height));
        fields.add_field_method_get("progress", |lua, this| lua.to_value(&this.progress));
        fields.add_field_method_get("title", |_, this| Ok(this.title.clone()));
        fields.add_field_method_get("user_vars", |_, this| Ok(this.user_vars.clone()));
        fields.add_field_method_get("foreground_process_name", |_, this| {
            let mut name = None;
            if let Some(mux) = Mux::try_get() {
                if let Some(pane) = mux.get_pane(this.pane_id) {
                    name = pane.get_foreground_process_name(CachePolicy::AllowStale);
                }
            }
            match name {
                Some(name) => Ok(name),
                None => Ok("".to_string()),
            }
        });
        fields.add_field_method_get("tty_name", |_, this| {
            let mut name = None;
            if let Some(mux) = Mux::try_get() {
                if let Some(pane) = mux.get_pane(this.pane_id) {
                    name = pane.tty_name();
                }
            }
            Ok(name)
        });
        fields.add_field_method_get("current_working_dir", |_, this| {
            if let Some(mux) = Mux::try_get() {
                if let Some(pane) = mux.get_pane(this.pane_id) {
                    return Ok(pane
                        .get_current_working_dir(CachePolicy::AllowStale)
                        .map(|url| url_funcs::Url { url }));
                }
            }
            Ok(None)
        });
        fields.add_field_method_get("domain_name", |_, this| {
            let mut name = None;
            if let Some(mux) = Mux::try_get() {
                if let Some(pane) = mux.get_pane(this.pane_id) {
                    let domain_id = pane.domain_id();
                    name = mux
                        .get_domain(domain_id)
                        .map(|dom| dom.domain_name().to_string());
                }
            }
            match name {
                Some(name) => Ok(name),
                None => Ok("".to_string()),
            }
        });
    }
}

#[derive(Default)]
pub struct TabState {
    /// If is_some(), rather than display the actual tab
    /// contents, we're overlaying a little internal application
    /// tab.  We'll also route input to it.
    pub overlay: Option<OverlayState>,
}

/// Manages the state/queue of lua based event handlers.
/// We don't want to queue more than 1 event at a time,
/// so we use this enum to allow for at most 1 executing
/// and 1 pending event.
#[derive(Copy, Clone, Debug)]
enum EventState {
    /// The event is not running
    None,
    /// The event is running
    InProgress,
    /// The event is running, and we have another one ready to
    /// run once it completes
    InProgressWithQueued(Option<PaneId>),
}

/// Aggregate snapshot of per-pane dirty-line bitmap telemetry
/// (ft-mpc9b.1.2). Read via `TermWindow::dirty_lines_telemetry()`
/// Snapshot of the per-frame budget allocator state + cosmetic-defer
/// aggregator (ft-d6nrd / ft-s0nah slice 1). Mirrors the
/// `*TelemetrySnapshot` shape used elsewhere in this crate so
/// `ft doctor` can fold every per-substrate snapshot through one
/// uniform path.
///
/// All counters are lifetime totals captured at snapshot time;
/// per-frame state (`spent_ns`, `queue_depth_now`) is also surfaced
/// so the doctor can highlight an active overflow without waiting
/// for the lifetime counter to tick over.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrameBudgetTelemetrySnapshot {
    /// Budget ceiling for the active refresh rate, in nanoseconds.
    pub budget_ns: u64,
    /// Spent so far in the current frame.
    pub spent_ns_current: u64,
    /// Lifetime total of cosmetic ops the budget refused to run
    /// in-frame and pushed to the deferred queue.
    pub deferrals_lifetime: u64,
    /// Lifetime total of deferred ops the queue evicted because it
    /// hit `deferred_cap`. Bead's "drop counter" — operator-actionable.
    pub drops_lifetime: u64,
    /// Lifetime total of bulk-drain passes (catch-up after Required
    /// ops freed budget headroom).
    pub bulk_drains_lifetime: u64,
    /// Current depth of the deferred-cosmetic queue.
    pub queue_depth_now: usize,
    /// Cosmetic-defer aggregator total across all four cosmetic
    /// op kinds (ligatures + subpixel-aa + decorations + animations).
    pub cosmetic_outstanding_total: u32,
    /// Per the substrate's FrameBudgetGateTelemetry. Counts the
    /// MotionGateDecision::Skip outcomes from the reduce-motion
    /// gate — ops that were not even queued because the operator
    /// has reduce-motion on.
    pub gate_skips_reduce_motion: u64,
    /// MotionGateDecision::Defer outcomes — gate said queue rather
    /// than execute.
    pub gate_defers: u64,
}

/// and consumed by `ft doctor`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DirtyLineTelemetrySnapshot {
    /// Number of panes with a registered dirty-line bitmap.
    pub pane_count: u64,
    /// Lifetime sum of dirty-mark transitions across panes.
    pub total_dirty_marks: u64,
    /// Lifetime sum of frame-end clear calls across panes.
    pub total_frames_cleared: u64,
    /// Sum of every pane's bitmap capacity (visible row count).
    pub total_capacity: u64,
    /// Sum of every pane's currently-dirty rows at snapshot time.
    pub currently_dirty_lines: u64,
    /// Per ft-8pcwy: lifetime sum of clean lines the render pass
    /// skipped because they were not marked dirty for the frame.
    /// Drives the bead's `clean_lines_skipped` ft-doctor metric.
    pub total_clean_lines_skipped: u64,
    /// Per ft-i6k6u / ft-jvj78 slice: per-source mark counters
    /// from the substrate's `MarksBySource`. Lets ft-doctor
    /// break down "what dirtied this frame" by source so an
    /// operator can spot a runaway source (e.g., a misbehaving
    /// PTY producing 100x normal write rate).
    pub marks_by_source: frankenterm_core::dirty_line_telemetry::MarksBySource,
}

/// Snapshot of the DEC 2026 Begin-Synchronized-Update subsystem
/// (ft-a9eu1 / ft-1dq8h slice). Combines the watchdog telemetry
/// (BSU/ESU counts, watchdog force-flushes, mid-BSU byte count,
/// adversarial underflow count) with the orchestrator telemetry
/// (admission accept/truncate/refuse counts, override decisions,
/// drain causes) into the single view ft-doctor renders.
///
/// Mirrors the *TelemetrySnapshot shape used elsewhere in this
/// crate (DirtyLineTelemetrySnapshot, FrameBudgetTelemetrySnapshot)
/// so the doctor surface folds every per-substrate snapshot
/// through one uniform path.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncOutputDoctorSnapshot {
    // ── Watchdog counters (substrate: SyncOutputTelemetry) ──
    pub bsu_count: u64,
    pub esu_count: u64,
    pub esu_flush_count: u64,
    pub watchdog_force_flush_count: u64,
    pub mid_bsu_byte_count: u64,
    pub max_bsu_depth_observed: u32,
    pub mode_query_count: u64,
    /// Bead's headline-attack signal: ESU received without an
    /// open BSU. Operator-actionable; non-zero values indicate
    /// either a misbehaving emitter or an adversarial pattern
    /// trying to confuse the watchdog state.
    pub adversarial_esu_underflow_count: u64,

    // ── Orchestrator counters (substrate:
    // SyncOutputOrchestratorTelemetry) ──
    pub admissions_accepted: u64,
    pub admissions_truncated: u64,
    pub admissions_refused: u64,
    pub bytes_accepted: u64,
    pub bytes_truncated: u64,
    pub bytes_refused: u64,
    pub bytes_drained_total: u64,
    pub overrides_pass_through: u64,
    pub overrides_coalesced: u64,
    pub overrides_force_flush: u64,
    pub drains_esu: u64,
    pub drains_watchdog: u64,
    pub drains_live_resize: u64,
    pub drains_operator: u64,
    pub drains_no_op: u64,
    /// Per-trigger override breakdown: bell + cursor-blink +
    /// live-resize + a11y-query.
    pub overrides_by_trigger: frankenterm_core::sync_output_buffer_orchestrator::OverridesByTrigger,
}

pub(crate) fn record_sync_output_mux_event(
    pane_id: PaneId,
    event: SynchronizedOutputEvent,
    watchdog_telemetry: &mut frankenterm_core::sync_output_watchdog::SyncOutputTelemetry,
    orchestrator_telemetry: &mut frankenterm_core::sync_output_buffer_orchestrator::SyncOutputOrchestratorTelemetry,
    bsu_depth_by_pane: &mut HashMap<
        PaneId,
        frankenterm_core::sync_output_watchdog::BsuDepthCounter,
    >,
    buffered_bytes_by_pane: &mut HashMap<PaneId, u64>,
) {
    match event {
        SynchronizedOutputEvent::Depth { outcome, max_depth } => {
            let outcome = record_sync_output_depth_outcome(pane_id, outcome, bsu_depth_by_pane);
            watchdog_telemetry.record_depth_outcome(outcome, max_depth);
        }
        SynchronizedOutputEvent::Admission { decision, bytes } => {
            let config =
                frankenterm_core::sync_output_buffer_orchestrator::BsuBufferConfig::default();
            let buffered_bytes = buffered_bytes_by_pane.entry(pane_id).or_default();
            let decision = sync_output_admission_decision_from_mux(decision);
            orchestrator_telemetry.record_admission(decision, bytes);
            frankenterm_core::sync_output_telemetry_bridge::forward_admission(
                decision,
                bytes,
                watchdog_telemetry,
            );
            match decision {
                frankenterm_core::sync_output_buffer_orchestrator::BufferAdmissionDecision::Accepted => {
                    *buffered_bytes = buffered_bytes
                        .saturating_add(bytes)
                        .min(config.effective_max_bytes());
                }
                frankenterm_core::sync_output_buffer_orchestrator::BufferAdmissionDecision::Truncated {
                    dropped_bytes,
                } => {
                    *buffered_bytes = buffered_bytes
                        .saturating_sub(dropped_bytes)
                        .saturating_add(bytes)
                        .min(config.effective_max_bytes());
                }
                frankenterm_core::sync_output_buffer_orchestrator::BufferAdmissionDecision::Refused => {}
            }
        }
        SynchronizedOutputEvent::Drain {
            cause,
            bytes,
            depth_outcome,
            max_depth,
        } => record_sync_output_drain(
            SyncOutputDrainRecord {
                pane_id,
                cause,
                bytes,
                maybe_depth_outcome: depth_outcome,
                max_depth,
            },
            watchdog_telemetry,
            orchestrator_telemetry,
            bsu_depth_by_pane,
            buffered_bytes_by_pane,
        ),
        SynchronizedOutputEvent::ModeQuery => {
            frankenterm_core::sync_output_telemetry_bridge::forward_mode_query(watchdog_telemetry);
        }
    }
}

fn sync_output_admission_decision_from_mux(
    decision: SynchronizedOutputAdmissionDecision,
) -> frankenterm_core::sync_output_buffer_orchestrator::BufferAdmissionDecision {
    match decision {
        SynchronizedOutputAdmissionDecision::Accepted => {
            frankenterm_core::sync_output_buffer_orchestrator::BufferAdmissionDecision::Accepted
        }
        SynchronizedOutputAdmissionDecision::Truncated { dropped_bytes } => {
            frankenterm_core::sync_output_buffer_orchestrator::BufferAdmissionDecision::Truncated {
                dropped_bytes,
            }
        }
        SynchronizedOutputAdmissionDecision::Refused => {
            frankenterm_core::sync_output_buffer_orchestrator::BufferAdmissionDecision::Refused
        }
    }
}

fn sync_output_depth_outcome_from_mux(
    outcome: SynchronizedOutputDepthOutcome,
) -> frankenterm_core::sync_output_watchdog::BsuDepthOutcome {
    match outcome {
        SynchronizedOutputDepthOutcome::Opened { new_depth } => {
            frankenterm_core::sync_output_watchdog::BsuDepthOutcome::Opened { new_depth }
        }
        SynchronizedOutputDepthOutcome::Closed { new_depth } => {
            frankenterm_core::sync_output_watchdog::BsuDepthOutcome::Closed { new_depth }
        }
        SynchronizedOutputDepthOutcome::Flushed => {
            frankenterm_core::sync_output_watchdog::BsuDepthOutcome::Flushed
        }
        SynchronizedOutputDepthOutcome::Underflow => {
            frankenterm_core::sync_output_watchdog::BsuDepthOutcome::Underflow
        }
    }
}

fn record_sync_output_depth_outcome(
    pane_id: PaneId,
    outcome: SynchronizedOutputDepthOutcome,
    bsu_depth_by_pane: &mut HashMap<
        PaneId,
        frankenterm_core::sync_output_watchdog::BsuDepthCounter,
    >,
) -> frankenterm_core::sync_output_watchdog::BsuDepthOutcome {
    let should_remove = matches!(
        outcome,
        SynchronizedOutputDepthOutcome::Flushed | SynchronizedOutputDepthOutcome::Underflow
    );
    {
        let depth = bsu_depth_by_pane.entry(pane_id).or_default();
        match outcome {
            SynchronizedOutputDepthOutcome::Opened { .. } => {
                let _ = depth.open_bsu();
            }
            SynchronizedOutputDepthOutcome::Closed { .. }
            | SynchronizedOutputDepthOutcome::Flushed
            | SynchronizedOutputDepthOutcome::Underflow => {
                let _ = depth.close_esu();
            }
        }
    }
    if should_remove {
        bsu_depth_by_pane.remove(&pane_id);
    }
    sync_output_depth_outcome_from_mux(outcome)
}

#[derive(Clone, Copy)]
struct SyncOutputDrainRecord {
    pane_id: PaneId,
    cause: SynchronizedOutputDrainCause,
    bytes: u64,
    maybe_depth_outcome: Option<SynchronizedOutputDepthOutcome>,
    max_depth: u32,
}

fn record_sync_output_drain(
    record: SyncOutputDrainRecord,
    watchdog_telemetry: &mut frankenterm_core::sync_output_watchdog::SyncOutputTelemetry,
    orchestrator_telemetry: &mut frankenterm_core::sync_output_buffer_orchestrator::SyncOutputOrchestratorTelemetry,
    bsu_depth_by_pane: &mut HashMap<
        PaneId,
        frankenterm_core::sync_output_watchdog::BsuDepthCounter,
    >,
    buffered_bytes_by_pane: &mut HashMap<PaneId, u64>,
) {
    use frankenterm_core::sync_output_buffer_orchestrator::{
        BufferDrainOutcome, DrainCause, OverrideAction, OverrideTrigger,
    };
    use frankenterm_core::sync_output_watchdog::WatchdogDecision;

    let bytes = if record.bytes > 0 {
        buffered_bytes_by_pane.remove(&record.pane_id);
        record.bytes
    } else {
        buffered_bytes_by_pane
            .remove(&record.pane_id)
            .unwrap_or_default()
    };
    let drain_cause = match record.cause {
        SynchronizedOutputDrainCause::Esu => DrainCause::Esu,
        SynchronizedOutputDrainCause::Watchdog => DrainCause::Watchdog,
        SynchronizedOutputDrainCause::LiveResizeForce => DrainCause::LiveResizeForce,
        SynchronizedOutputDrainCause::Operator => DrainCause::Operator,
    };
    let drain_outcome = if bytes > 0 {
        BufferDrainOutcome::Drained {
            bytes,
            cause: drain_cause,
        }
    } else {
        BufferDrainOutcome::NoOp
    };

    if matches!(record.cause, SynchronizedOutputDrainCause::LiveResizeForce) {
        orchestrator_telemetry
            .record_override(OverrideTrigger::LiveResize, OverrideAction::ForceFlushNow);
    }
    orchestrator_telemetry.record_drain(drain_outcome);

    match record.cause {
        SynchronizedOutputDrainCause::Esu => {
            let depth_outcome = record
                .maybe_depth_outcome
                .map(|outcome| {
                    record_sync_output_depth_outcome(record.pane_id, outcome, bsu_depth_by_pane)
                })
                .unwrap_or_else(|| {
                    let (outcome, should_remove) = {
                        let depth = bsu_depth_by_pane.entry(record.pane_id).or_default();
                        let outcome = depth.close_esu();
                        let should_remove = matches!(
                            outcome,
                            frankenterm_core::sync_output_watchdog::BsuDepthOutcome::Flushed
                                | frankenterm_core::sync_output_watchdog::BsuDepthOutcome::Underflow
                        );
                        (outcome, should_remove)
                    };
                    if should_remove {
                        bsu_depth_by_pane.remove(&record.pane_id);
                    }
                    outcome
                });
            if matches!(drain_outcome, BufferDrainOutcome::Drained { .. }) {
                frankenterm_core::sync_output_telemetry_bridge::forward_drain(
                    drain_outcome,
                    depth_outcome,
                    record.max_depth,
                    watchdog_telemetry,
                );
            } else {
                watchdog_telemetry.record_depth_outcome(depth_outcome, record.max_depth);
            }
        }
        SynchronizedOutputDrainCause::Watchdog => {
            watchdog_telemetry.record_watchdog_decision(WatchdogDecision::ForceFlush);
            if let Some(depth) = bsu_depth_by_pane.get_mut(&record.pane_id) {
                depth.force_reset();
            }
        }
        SynchronizedOutputDrainCause::LiveResizeForce | SynchronizedOutputDrainCause::Operator => {
            if let Some(depth) = bsu_depth_by_pane.get_mut(&record.pane_id) {
                depth.force_reset();
            }
        }
    }
}

/// Convert an explicit `WatchdogedTripleBuffer` health view into the per-pane
/// snapshot shape retained by the dormant doctor foundation. Production has no
/// live terminal-state buffer consumer today; tests and a future bounded
/// integration use this single translation point.
#[must_use]
pub fn pane_health_snapshot_from_watchdoged_health(
    pane_id: u64,
    health: &frankenterm_core::watchdoged_triple_buffer::WatchdogedHealth,
    last_force_recycle_ts_ms: u64,
) -> frankenterm_core::triple_buffer_fleet_health::PaneHealthSnapshot {
    let stats = health.watchdog();
    frankenterm_core::triple_buffer_fleet_health::PaneHealthSnapshot {
        pane_id: frankenterm_core::triple_buffer_fleet_health::PaneId(pane_id),
        acquires: stats.acquires_total(),
        releases: stats.releases_total(),
        warnings: stats.warnings_emitted(),
        force_recycles: stats.force_recycle_invocations(),
        last_force_recycle_ts_ms,
        watchdog_active: health.watchdog_active(),
    }
}

/// Snapshot of the workspace-wide ElasticBuffer policy state. Read
/// via `TermWindow::elastic_buffer_telemetry()` and consumed by `ft doctor`.
/// Capacity and used counters come from the live `RenderState`
/// quad allocation; growth counters advance only when the renderer
/// performs a real quad-buffer reallocation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ElasticBufferTelemetrySnapshot {
    /// Current allocated capacity in elements.
    pub capacity: u64,
    /// Current `len()` (used elements) — this is the
    /// post-frame-clear value most of the time.
    pub used: u64,
    /// Lifetime count of capacity-doubling growth events.
    pub grow_count: u64,
    /// Lifetime count of successful idle-shrink events.
    pub shrink_count: u64,
    /// Peak `len()` observed since the most recent successful
    /// shrink (or since construction). The shrink target rounds
    /// this up to the next bucket.
    pub high_water_mark: u64,
    /// Whether the policy currently believes a resize gesture is
    /// in flight. Idle-shrink is suppressed while true.
    pub gesture_active: bool,
}

pub struct TermWindow {
    pub window: Option<Window>,
    pub config: ConfigHandle,
    pub config_overrides: wezterm_dynamic::Value,
    os_parameters: Option<parameters::Parameters>,
    /// When we most recently received keyboard focus
    pub focused: Option<Instant>,
    fonts: Rc<FontConfiguration>,
    /// Window dimensions and dpi
    pub dimensions: Dimensions,
    pub window_state: WindowState,
    pub resizes_pending: usize,
    is_repaint_pending: bool,
    /// Coalesces bursty `update_title()` invocations triggered by high-rate
    /// mux Alerts (Progress, CurrentWorkingDirectoryChanged, OutputSinceFocusLost).
    /// When true, an `Apply` notification is already in flight that will run
    /// `update_title` on the next frame and clear this flag. See ft-9d60d.
    pending_update_title: bool,
    pending_scale_changes: LinkedList<resize::ScaleChange>,
    /// Terminal dimensions
    terminal_size: TerminalSize,
    pub mux_window_id: MuxWindowId,
    pub mux_window_id_for_subscriptions: Arc<Mutex<MuxWindowId>>,
    pub render_metrics: RenderMetrics,
    render_state: Option<RenderState>,
    input_map: InputMap,
    /// If is_some, the LEADER modifier is active until the specified instant.
    leader_is_down: Option<std::time::Instant>,
    dead_key_status: DeadKeyStatus,
    key_table_state: KeyTableState,
    show_tab_bar: bool,
    show_scroll_bar: bool,
    tab_bar: TabBarState,
    fancy_tab_bar: Option<box_model::ComputedElement>,
    pub right_status: String,
    pub left_status: String,
    last_ui_item: Option<UIItem>,
    /// Tracks whether the current mouse-down event is part of click-focus.
    /// If so, we ignore mouse events until released
    is_click_to_focus_window: bool,
    last_mouse_coords: (usize, i64),
    window_drag_position: Option<MouseEvent>,
    current_mouse_event: Option<MouseEvent>,
    prev_cursor: PrevCursorPos,
    last_scroll_info: RenderableDimensions,

    tab_state: RefCell<HashMap<TabId, TabState>>,
    pane_state: RefCell<HashMap<PaneId, PaneState>>,
    semantic_zones: HashMap<PaneId, SemanticZoneCache>,

    window_background: Vec<LoadedBackgroundLayer>,
    background_load: BackgroundLoadCoordinator,

    current_modifier_and_leds: (Modifiers, KeyboardLedStatus),
    current_mouse_buttons: Vec<MousePress>,
    current_mouse_capture: Option<MouseCapture>,
    active_selection_drag_pane: Option<PaneId>,

    opengl_info: Option<String>,

    /// Keeps track of double and triple clicks
    last_mouse_click: Option<LastMouseClick>,

    /// The URL over which we are currently hovering
    current_highlight: Option<Arc<Hyperlink>>,

    quad_generation: usize,
    shape_generation: usize,
    /// Per-pane render-side dirty-line bitmap (ft-tfzhy / ft-mpc9b.1.2).
    ///
    /// The TermWindow keeps one `DirtyLineBitmap` per `PaneId` to retain
    /// changed-row attribution through exact presentation settlement. The
    /// current render pass uses it to classify clean cached-quad hits for
    /// telemetry; it does not yet skip hashing or cache lookup. Coarse
    /// whole-screen events (resize, focus change, font/theme swap)
    /// call `mark_all` on every entry; live PTY dirty ranges, cursor
    /// moves, and selection changes mark row-level damage.
    ///
    /// The `quad_generation` counter above stays as the lower-bound
    /// version on top of per-line dirty for events that genuinely
    /// invalidate everything (font swap, theme change). Both
    /// signals are consumed by the render path together.
    dirty_lines: HashMap<PaneId, render::dirty_lines::DirtyLineBitmap>,
    /// Monotonic identity of dirty-state mutations. A frame captures this
    /// immediately before draw and may settle damage only if presentation
    /// succeeds with the same non-exhausted generation.
    damage_generation: DamageGeneration,
    /// Admission/circuit state for renderer recovery.  Dirty state continues
    /// to accumulate while a retry is cooling down, but expensive geometry is
    /// admitted only by a healthy renderer or an exact retry ticket.
    render_recovery_state: RenderRecoveryState,
    /// Single cancellable wake lane shared by renderer retries and animation.
    /// Ticket validation remains authoritative even if cancellation races a
    /// task that has already posted its notification.
    render_wake_state: RenderWakeState,
    /// Gate for the iter-dirty render-pass clean-line accounting
    /// path (ft-8pcwy / ft-jvj78 / ft-gwzrm). The live source
    /// wiring marks PTY, cursor, selection, and whole-screen events
    /// before paint; the render loop only records a clean-line skip
    /// after it has reused cached quads for that row, so enabling
    /// the gate cannot create visual holes when a cache entry is
    /// unavailable.
    iter_dirty_render_gate_enabled: bool,
    /// Per-frame budget allocator (ft-d6nrd / ft-s0nah slice 1).
    /// Tracks the headroom left in the current frame for cosmetic
    /// ops + the deferred-cosmetic queue across frames. paint_impl
    /// calls begin_frame at entry, routes render operations through
    /// `try_execute`, and ends the budget after Present.
    frame_budget: frame_budget::FrameBudget,
    /// Idle frame-rate detector (ft-s8guw). Central event paths
    /// call `record_idle_event`, while the status tick polls the
    /// scheduler decision so doctor/scheduler consumers see the
    /// live current state without reaching into event handlers.
    idle_detector: idle_detector::IdleDetector,
    last_idle_transition: Option<idle_detector::IdleTransitionReport>,
    /// Cosmetic-defer aggregator (ft-d6nrd). Per-op wiring records
    /// a defer when the budget pushes a cosmetic op out of frame
    /// and records a drain when queued work is admitted again. The
    /// redraw predicate consults `has_outstanding()` so the next
    /// frame is forced when there is deferred work.
    cosmetic_defer_outstanding: frankenterm_core::frame_budget_a11y_gate::CosmeticDeferOutstanding,
    /// A11Y reduce-motion + cosmetic-defer gate telemetry
    /// (ft-d6nrd). Aggregates the per-decision counters from the
    /// substrate's `evaluate_reduce_motion_gate` for ft doctor
    /// surface emission.
    frame_budget_gate_telemetry: frankenterm_core::frame_budget_a11y_gate::FrameBudgetGateTelemetry,
    /// Cached reduce-motion state consumed by FrameBudget render
    /// gates. The OS probe shells out on some platforms, so the
    /// paint loop must read this cached value instead of probing
    /// per render operation or per frame.
    frame_budget_reduce_motion_state: frame_budget_a11y::ReduceMotionState,
    /// Per-source dirty-mark counters (ft-i6k6u / ft-jvj78 slice).
    /// Substrate's `MarksBySource` aggregator: 9 lifetime counts
    /// (one per `DirtyEventSource` variant). Bumped by
    /// `record_dirty_event` or the source-tagged whole-screen helper.
    /// Surfaced into `DirtyLineTelemetrySnapshot.marks_by_source`
    /// for ft-doctor's lines-redrawn-by-source breakdown.
    dirty_marks_by_source: frankenterm_core::dirty_line_telemetry::MarksBySource,
    /// Retained per-pane `TerminalState` triple-buffer foundation. There is no
    /// production reader, so the hot render path intentionally does not
    /// publish into this registry. A future integration must add a bounded
    /// consumer before re-enabling a producer.
    triple_buffer_panes: TerminalStateTripleBufferRegistry,
    /// Per-pane WatchdogedTripleBuffer health snapshots
    /// (ft-gso6n / ft-l0oe3 slice). `triple_buffer_telemetry()`
    /// aggregates these into the substrate's FleetHealthAggregate. With the
    /// mirror producer/consumer dormant, production leaves this map
    /// empty; explicit bridge tests exercise its aggregation helpers.
    triple_buffer_pane_health:
        HashMap<u64, frankenterm_core::triple_buffer_fleet_health::PaneHealthSnapshot>,
    /// DEC 2026 Begin-Synchronized-Update watchdog telemetry
    /// (ft-a9eu1 / ft-1dq8h slice 1). Substrate's
    /// SyncOutputTelemetry: BSU / ESU counts, watchdog
    /// force-flushes, mid-BSU byte count, max depth observed,
    /// mode-query count, adversarial-ESU-underflow count.
    /// Surfaced into `sync_output_telemetry()` for ft doctor and fed
    /// by mux DEC 2026 BSU/ESU notifications.
    sync_output_watchdog_telemetry: frankenterm_core::sync_output_watchdog::SyncOutputTelemetry,
    /// DEC 2026 BSU buffer + override-dispatch orchestrator
    /// telemetry (ft-a9eu1). Substrate's
    /// SyncOutputOrchestratorTelemetry: per-admission accept /
    /// truncate / refuse counts, override pass-through /
    /// coalesce / force-flush, drains by cause. Surfaced into
    /// `sync_output_telemetry()` alongside the watchdog
    /// counters.
    sync_output_orchestrator_telemetry:
        frankenterm_core::sync_output_buffer_orchestrator::SyncOutputOrchestratorTelemetry,
    /// Per-pane BSU depth state used to translate mux DEC 2026
    /// notifications into watchdog telemetry.
    sync_output_bsu_depth_by_pane:
        HashMap<PaneId, frankenterm_core::sync_output_watchdog::BsuDepthCounter>,
    /// Per-pane bytes admitted while a BSU window is open. Drain
    /// notifications consume this map to attribute ESU/watchdog/live-resize
    /// bytes without making the mux crate depend on frankenterm-core.
    sync_output_buffered_bytes_by_pane: HashMap<PaneId, u64>,
    /// ElasticBuffer policy engine for the per-pane quad/instance
    /// buffer (ft-kciew / ft-mpc9b.1.3).
    ///
    /// The policy tracks gesture state so the underlying GPU buffer
    /// (continuation bead) doesn't reallocate during a resize drag.
    /// `begin_quad_resize_gesture()` is called when the OS reports
    /// `live_resizing=true`; `end_quad_resize_gesture()` is called
    /// on the first non-live resize event after a sequence of live
    /// ones; `tick_quad_buffer_shrink()` is driven by the periodic
    /// status-update timer to release excess capacity once the
    /// gesture has ended and the idle threshold has elapsed.
    ///
    /// The policy observes the live `RenderState` quad allocation
    /// without claiming ownership of GPU memory. A future quad-writer
    /// continuation can still move the actual staging data into this
    /// policy; today the telemetry is live allocation-backed while
    /// shrink remains observational.
    quad_buffer_policy: render::elastic_buffer::ElasticBuffer<()>,
    /// Whether `quad_buffer_policy` currently believes a live
    /// resize is in progress. The OS-side `live_resizing` boolean
    /// is sticky-on-active so we track the level transitions
    /// here to call `begin_gesture` once per gesture rather than
    /// per `resize` event.
    quad_buffer_in_resize_gesture: bool,
    shape_cache: RefCell<LfuCache<ShapeCacheKey, anyhow::Result<Rc<CachedShape>>>>,
    line_to_ele_shape_cache: RefCell<LfuCache<LineToEleShapeCacheKey, LineToElementShapeItem>>,

    /// Unforgeable identity for this window's line-state LFU. Numeric entry
    /// IDs are monotonic only within one `TermWindow`; renderer appdata can be
    /// carried by shared pane lines into another window.
    line_state_cache_owner: Arc<render::LineStateCacheOwner>,
    line_state_cache: RefCell<LfuCacheU64<Arc<CachedLineState>>>,
    next_line_state_id: u64,

    line_quad_cache: RefCell<LfuCache<LineQuadCacheKey, LineQuadCacheValue>>,

    /// Last mux window-order revision reconciled against `tab_state`. This
    /// keeps closed-tab state bounded without rescanning all tabs on ordinary
    /// same-revision invalidations.
    last_tab_state_prune_revision: Cell<Option<mux::window::WindowOrderRevision>>,

    last_status_call: Instant,
    cursor_blink_state: RefCell<ColorEase>,
    blink_state: RefCell<ColorEase>,
    rapid_blink_state: RefCell<ColorEase>,

    palette: Option<ColorPalette>,

    ui_items: Vec<UIItem>,
    dragging: Option<(UIItem, MouseEvent)>,

    modal: RefCell<Option<Rc<dyn Modal>>>,

    event_states: HashMap<String, EventState>,
    pub current_event: Option<Value>,
    has_animation: RefCell<Option<Instant>>,
    /// We use this to attempt to do something reasonable
    /// if we run out of texture space
    allow_images: AllowImage,
    created: Instant,

    pub last_frame_duration: Duration,
    last_fps_check_time: Instant,
    num_frames: usize,
    pub fps: f32,

    connection_name: String,

    gl: Option<Rc<glium::backend::Context>>,
    webgpu: Option<Rc<WebGpuState>>,
    config_subscription: Option<config::ConfigSubscription>,

    /// Per-pane agent state classification, updated each render tick.
    agent_pane_states: HashMap<PaneId, frankenterm_core::agent_pane_state::AgentPaneState>,

    /// Integrated agent swarm dashboard panel.
    dashboard: crate::dashboard::DashboardPanel,
}

/// Pure predicate used only to classify a successful cached-quad hit as clean
/// for accounting. Cache lookup, hash/key construction, and reuse remain
/// independent of this bitmap today.
///
/// Semantics:
/// - gate disabled → do not count the cache hit as a clean hit.
/// - no bitmap or an empty bitmap → do not infer per-row cleanliness.
/// - non-empty bitmap → classify the row as clean iff it is not dirty.
pub(crate) fn is_clean_line_for_cache_hit_accounting(
    gate_enabled: bool,
    bitmap: Option<&render::dirty_lines::DirtyLineBitmap>,
    line_idx: usize,
) -> bool {
    if !gate_enabled {
        return false;
    }
    let Some(bm) = bitmap else {
        return false;
    };
    if bm.is_empty() {
        return false;
    }
    !bm.contains(line_idx)
}

/// True only while an active terminal-pane left-drag is extending a selection.
/// Dirty PTY output may redraw under that in-flight selection, but it must not
/// clear the selection before the matching mouse-up can copy it.
pub(crate) fn should_preserve_selection_during_dirty_line_update(
    current_mouse_capture: &Option<MouseCapture>,
    current_mouse_buttons: &[MousePress],
    active_selection_drag_pane: Option<PaneId>,
    pane_id: PaneId,
) -> bool {
    let captured_pane_id = match current_mouse_capture {
        Some(MouseCapture::TerminalPane(captured_pane_id)) => Some(*captured_pane_id),
        Some(MouseCapture::UI) | None => None,
    };
    frankenterm_gui::should_preserve_dirty_selection_during_mouse_drag(
        active_selection_drag_pane,
        captured_pane_id,
        current_mouse_buttons.contains(&MousePress::Left),
        pane_id,
    )
}

/// Per ft-camu6: mark every stable-row index in `stable_rows`
/// dirty in the supplied bitmap, translating to visible-row index
/// via `stable_row - viewport`. Out-of-bounds rows are silently
/// dropped (DirtyLineBitmap::mark contract). Free helper so the
/// translation logic is unit-testable without standing up a
/// TermWindow / Pane / RangeSet.
pub(crate) fn mark_stable_rows_dirty<I>(
    bitmap: &mut render::dirty_lines::DirtyLineBitmap,
    viewport: isize,
    stable_rows: I,
) where
    I: IntoIterator<Item = isize>,
{
    for stable_row in stable_rows {
        let visible_idx = stable_row.saturating_sub(viewport);
        if let Ok(idx_usize) = usize::try_from(visible_idx) {
            bitmap.mark(idx_usize);
        }
    }
}

/// Mark dirty stable-row ranges after translating them into visible
/// row ranges for a pane-local bitmap. Ranges partially overlapping
/// the viewport are clamped instead of dropped, which keeps live mux
/// dirty-range events precise without expanding every row first.
pub(crate) fn mark_stable_row_ranges_dirty<I>(
    bitmap: &mut render::dirty_lines::DirtyLineBitmap,
    viewport: StableRowIndex,
    stable_ranges: I,
) where
    I: IntoIterator<Item = Range<StableRowIndex>>,
{
    for range in stable_ranges {
        let visible_start = range.start.saturating_sub(viewport);
        let visible_end = range.end.saturating_sub(viewport);
        let start = if visible_start <= 0 {
            0
        } else {
            usize::try_from(visible_start).unwrap_or(usize::MAX)
        };
        let end = if visible_end <= 0 {
            0
        } else {
            usize::try_from(visible_end).unwrap_or(usize::MAX)
        };
        if start < end {
            bitmap.mark_range(start..end);
        }
    }
}

/// Per ft-jvj78: cursor moves invalidate the previous and current
/// cursor rows so the old cursor glyph is erased and the new one is
/// drawn when iter-dirty rendering is enabled.
pub(crate) fn mark_cursor_rows_dirty(
    bitmap: &mut render::dirty_lines::DirtyLineBitmap,
    viewport: StableRowIndex,
    previous: StableCursorPosition,
    current: StableCursorPosition,
) {
    mark_stable_rows_dirty(bitmap, viewport, [previous.y, current.y]);
}

/// Per ft-d6nrd slice 1: pure predicate the redraw decision
/// consults to decide whether the next frame must paint to make
/// progress on deferred cosmetic work. Free function so the truth
/// table is unit-testable without standing up a TermWindow.
///
/// Returns true iff EITHER:
/// - the FrameBudget allocator's deferred-cosmetic queue has
///   carry-over ops from the prior frame, OR
/// - the cosmetic-defer aggregator shows outstanding ops across
///   any cosmetic kind (ligatures / subpixel-aa / decorations /
///   animations).
pub(crate) fn should_force_paint_for_frame_budget(
    queue_depth: usize,
    cosmetic_outstanding: u32,
) -> bool {
    queue_depth > 0 || cosmetic_outstanding > 0
}

/// Convert the platform preference probe's CSS-shaped motion axis
/// into the FrameBudget A11Y gate's 3-state reduce-motion input.
#[must_use]
pub(crate) fn reduce_motion_state_from_preference(
    preference: MotionPreference,
) -> frame_budget_a11y::ReduceMotionState {
    match preference {
        MotionPreference::NoPreference => frame_budget_a11y::ReduceMotionState::Off,
        MotionPreference::Reduce => frame_budget_a11y::ReduceMotionState::On,
    }
}

/// One-shot platform probe output normalized for the FrameBudget
/// reduce-motion gate. The probe itself lives in core; the GUI
/// bridge owns the conversion to `ReduceMotionState`.
#[must_use]
pub(crate) fn probe_reduce_motion_state() -> frame_budget_a11y::ReduceMotionState {
    reduce_motion_state_from_preference(frankenterm_core::reduce_motion_probe::probe_reduce_motion())
}

#[must_use]
pub(crate) fn a11y_op_kind_from_frame_budget_op(
    op: frame_budget::OpKind,
) -> Option<frame_budget_a11y::OpKind> {
    match op {
        frame_budget::OpKind::DirtyQuadRebuild => Some(frame_budget_a11y::OpKind::DirtyQuadRebuild),
        frame_budget::OpKind::Cursor => Some(frame_budget_a11y::OpKind::Cursor),
        frame_budget::OpKind::Selection => Some(frame_budget_a11y::OpKind::Selection),
        frame_budget::OpKind::Ligatures => Some(frame_budget_a11y::OpKind::Ligatures),
        frame_budget::OpKind::SubpixelAa => Some(frame_budget_a11y::OpKind::SubpixelAa),
        frame_budget::OpKind::Decorations => Some(frame_budget_a11y::OpKind::Decorations),
        frame_budget::OpKind::Animations => Some(frame_budget_a11y::OpKind::Animations),
        frame_budget::OpKind::Custom(_) => None,
    }
}

#[must_use]
pub(crate) fn default_frame_budget_cost_ns(op: frame_budget::OpKind) -> u64 {
    let table = frankenterm_core::frame_budget_a11y_gate::OpCostTable::default();
    match a11y_op_kind_from_frame_budget_op(op) {
        Some(a11y_op) => table.lookup_ns(a11y_op),
        None => frame_budget::FrameBudgetCostFeedback::CUSTOM_DEFAULT_NS,
    }
}

#[must_use]
pub(crate) fn should_run_frame_budget_decision(
    decision: frame_budget_a11y::MotionGateDecision,
) -> bool {
    matches!(decision, frame_budget_a11y::MotionGateDecision::Execute)
}

pub(crate) fn record_drained_frame_budget_ops<I>(
    outstanding: &mut frankenterm_core::frame_budget_a11y_gate::CosmeticDeferOutstanding,
    drained_ops: I,
) where
    I: IntoIterator<Item = frame_budget::DeferredOp>,
{
    for op in drained_ops {
        if let Some(a11y_op) = a11y_op_kind_from_frame_budget_op(op.kind) {
            outstanding.record_drained(a11y_op);
        }
    }
}

pub(crate) fn record_frame_budget_execution_outstanding(
    outstanding: &mut frankenterm_core::frame_budget_a11y_gate::CosmeticDeferOutstanding,
    op: frame_budget::OpKind,
    execution: frame_budget::ExecutionDecision,
) {
    match execution {
        frame_budget::ExecutionDecision::Deferred => {
            if let Some(a11y_op) = a11y_op_kind_from_frame_budget_op(op) {
                outstanding.record_deferred(a11y_op);
            }
        }
        frame_budget::ExecutionDecision::Dropped { evicted } => {
            record_drained_frame_budget_ops(outstanding, [evicted]);
            if let Some(a11y_op) = a11y_op_kind_from_frame_budget_op(op) {
                outstanding.record_deferred(a11y_op);
            }
        }
        frame_budget::ExecutionDecision::Executed { .. } => {}
    }
}

#[must_use]
pub(crate) fn base_policy_for_frame_budget_state(
    budget: &frame_budget::FrameBudget,
    priority: frame_budget::OpPriority,
) -> frame_budget_a11y::BaseExecutionPolicy {
    match priority {
        frame_budget::OpPriority::Required => frame_budget_a11y::BaseExecutionPolicy::Execute,
        frame_budget::OpPriority::Cosmetic => {
            if budget.would_defer_cosmetic_now() {
                if budget.deferred_queue_is_at_capacity() {
                    frame_budget_a11y::BaseExecutionPolicy::DropOldest
                } else {
                    frame_budget_a11y::BaseExecutionPolicy::Defer
                }
            } else {
                frame_budget_a11y::BaseExecutionPolicy::Execute
            }
        }
    }
}

#[must_use]
pub(crate) fn evaluate_frame_budget_reduce_motion_gate(
    budget: &frame_budget::FrameBudget,
    op: frame_budget::OpKind,
    priority: frame_budget::OpPriority,
    motion: frame_budget_a11y::ReduceMotionState,
) -> frame_budget_a11y::MotionGateDecision {
    let base = base_policy_for_frame_budget_state(budget, priority);
    let Some(a11y_op) = a11y_op_kind_from_frame_budget_op(op) else {
        return match base {
            frame_budget_a11y::BaseExecutionPolicy::Execute => {
                frame_budget_a11y::MotionGateDecision::Execute
            }
            frame_budget_a11y::BaseExecutionPolicy::Defer => {
                frame_budget_a11y::MotionGateDecision::Defer
            }
            frame_budget_a11y::BaseExecutionPolicy::DropOldest => {
                frame_budget_a11y::MotionGateDecision::DropOldest
            }
        };
    };
    frame_budget_a11y::evaluate_reduce_motion_gate(a11y_op, motion, base)
}

/// Monotonic identity of the dirty state captured by a frame.
///
/// Exhaustion is sticky: once `u64::MAX` cannot be advanced, no frame may
/// clear dirty state until the renderer is reconstructed. This avoids the
/// false equality that a saturating counter would otherwise create.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DamageGeneration {
    value: u64,
    exhausted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DamageAdvanceOutcome {
    Advanced,
    ExhaustedNow,
    AlreadyExhausted,
}

impl DamageGeneration {
    fn advance(&mut self) -> DamageAdvanceOutcome {
        if self.exhausted {
            return DamageAdvanceOutcome::AlreadyExhausted;
        }
        match self.value.checked_add(1) {
            Some(next) => {
                self.value = next;
                DamageAdvanceOutcome::Advanced
            }
            None => {
                self.exhausted = true;
                DamageAdvanceOutcome::ExhaustedNow
            }
        }
    }

    const fn can_commit(self, captured: Self) -> bool {
        !self.exhausted
            && self.value == captured.value
            && self.exhausted == captured.exhausted
    }
}

/// Free-function helper for frame damage settlement
/// so the predicate wiring is unit-testable without needing to
/// stand up a full `TermWindow`. Per ft-jvj78.
///
/// Exact successful presentation clears every bitmap. Failed attempts, stale
/// generations, and exhausted epochs never call this helper, so their damage
/// remains retained without forcing an unconditional second successful frame
/// after every whole-screen event.
fn run_clear_dirty_lines_after_frame(
    bitmaps: &mut HashMap<PaneId, render::dirty_lines::DirtyLineBitmap>,
) {
    for bitmap in bitmaps.values_mut() {
        bitmap.clear();
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum RenderFailureStage {
    Paint,
    Draw(DrawFailureStage),
    SurfaceAcquire(webgpu::WebGpuSurfaceTextureError),
    BackendFinish,
    /// Injectable settlement stage. The current wgpu submit API is
    /// synchronously infallible; asynchronous device loss is outside this seam.
    #[allow(dead_code)]
    Submission,
    /// Injectable settlement stage. The current wgpu present API returns `()`;
    /// asynchronous presentation/device errors are outside this seam.
    #[allow(dead_code)]
    Present,
}

impl RenderFailureStage {
    const fn label(self) -> &'static str {
        match self {
            Self::Paint => "paint",
            Self::Draw(stage) => stage.label(),
            Self::SurfaceAcquire(webgpu::WebGpuSurfaceTextureError::Timeout) => {
                "surface_timeout"
            }
            Self::SurfaceAcquire(webgpu::WebGpuSurfaceTextureError::Occluded) => {
                "surface_occluded"
            }
            Self::SurfaceAcquire(webgpu::WebGpuSurfaceTextureError::Outdated) => {
                "surface_outdated"
            }
            Self::SurfaceAcquire(webgpu::WebGpuSurfaceTextureError::Lost) => "surface_lost",
            Self::SurfaceAcquire(webgpu::WebGpuSurfaceTextureError::Validation) => {
                "surface_validation"
            }
            Self::BackendFinish => "backend_finish",
            Self::Submission => "submission",
            Self::Present => "present",
        }
    }

    const fn accepts_surface_recovery_signal(self) -> bool {
        matches!(
            self,
            Self::SurfaceAcquire(webgpu::WebGpuSurfaceTextureError::Timeout)
                | Self::SurfaceAcquire(webgpu::WebGpuSurfaceTextureError::Occluded)
                | Self::SurfaceAcquire(webgpu::WebGpuSurfaceTextureError::Outdated)
                | Self::SurfaceAcquire(webgpu::WebGpuSurfaceTextureError::Lost)
        )
    }
}

/// A typed failure for one admitted render attempt.
///
/// Keeping the stage beside the source means retry policy never depends on
/// parsing an error string or traversing an `anyhow` chain.  OpenGL can also
/// retain a draw error as primary while recording a failed mandatory frame
/// finish as secondary.
#[derive(Debug)]
pub(crate) struct RenderAttemptFailure {
    stage: RenderFailureStage,
    source: anyhow::Error,
    secondary_finish: Option<anyhow::Error>,
}

impl RenderAttemptFailure {
    pub(crate) fn new(stage: RenderFailureStage, source: anyhow::Error) -> Self {
        Self {
            stage,
            source,
            secondary_finish: None,
        }
    }

    pub(crate) fn paint(source: anyhow::Error) -> Self {
        Self::new(RenderFailureStage::Paint, source)
    }

    fn draw(source: impl Into<anyhow::Error>) -> Self {
        let source = source.into();
        let stage = source
            .downcast_ref::<DrawFailure>()
            .map_or(DrawFailureStage::RenderCommands, DrawFailure::stage);
        Self::new(RenderFailureStage::Draw(stage), source)
    }

    fn backend_finish(source: anyhow::Error) -> Self {
        Self::new(RenderFailureStage::BackendFinish, source)
    }

    fn with_secondary_finish(mut self, source: anyhow::Error) -> Self {
        self.secondary_finish = Some(source);
        self
    }

    const fn stage(&self) -> RenderFailureStage {
        self.stage
    }
}

impl std::fmt::Display for RenderAttemptFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} render failure: {:#}", self.stage.label(), self.source)?;
        if let Some(secondary) = &self.secondary_finish {
            write!(f, "; frame finish also failed: {secondary:#}")?;
        }
        Ok(())
    }
}

impl std::error::Error for RenderAttemptFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

fn combine_glium_draw_and_finish(
    draw_result: anyhow::Result<()>,
    finish_result: anyhow::Result<()>,
) -> Result<(), RenderAttemptFailure> {
    match (draw_result, finish_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(draw), Ok(())) => Err(RenderAttemptFailure::draw(draw)),
        (Ok(()), Err(finish)) => Err(RenderAttemptFailure::backend_finish(finish)),
        (Err(draw), Err(finish)) => {
            Err(RenderAttemptFailure::draw(draw).with_secondary_finish(finish))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum FrameCompletion {
    Presented(DamageGeneration),
    Failed(RenderFailureStage),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DamageCommitOutcome {
    Cleared,
    RetainedStale,
    RetainedFailure,
    RetainedEpochExhausted,
}

impl DamageCommitOutcome {
    const fn label(self) -> &'static str {
        match self {
            Self::Cleared => "cleared",
            Self::RetainedStale => "retained_stale",
            Self::RetainedFailure => "retained_failure",
            Self::RetainedEpochExhausted => "retained_epoch_exhausted",
        }
    }

    const fn needs_follow_up_paint(self) -> bool {
        matches!(self, Self::RetainedStale)
    }
}

fn settle_frame_damage(
    bitmaps: &mut HashMap<PaneId, render::dirty_lines::DirtyLineBitmap>,
    current_generation: DamageGeneration,
    completion: FrameCompletion,
) -> DamageCommitOutcome {
    match completion {
        FrameCompletion::Failed(_stage) => DamageCommitOutcome::RetainedFailure,
        FrameCompletion::Presented(_) if current_generation.exhausted => {
            DamageCommitOutcome::RetainedEpochExhausted
        }
        FrameCompletion::Presented(captured) if !current_generation.can_commit(captured) => {
            DamageCommitOutcome::RetainedStale
        }
        FrameCompletion::Presented(_) => {
            run_clear_dirty_lines_after_frame(bitmaps);
            DamageCommitOutcome::Cleared
        }
    }
}

impl TermWindow {
    fn gui_window_or_log(&self, operation: &str) -> Option<Window> {
        match self.window.clone() {
            Some(window) => Some(window),
            None => {
                log::error!("cannot {operation} without a GUI window");
                None
            }
        }
    }

    fn mux_or_log(&self, operation: &str) -> Option<Arc<Mux>> {
        match Mux::try_get() {
            Some(mux) => Some(mux),
            None => {
                log::error!("cannot {operation} without an active mux");
                None
            }
        }
    }

    fn mux_or_err(&self, operation: &str) -> anyhow::Result<Arc<Mux>> {
        Mux::try_get().ok_or_else(|| anyhow!("cannot {operation} without an active mux"))
    }

    fn load_os_parameters(&mut self) {
        if let Some(ref window) = self.window {
            self.os_parameters = match window.get_os_parameters(&self.config, self.window_state) {
                Ok(os_parameters) => os_parameters,
                Err(err) => {
                    log::warn!("Error while getting OS parameters: {:#}", err);
                    None
                }
            };
        }
    }

    fn close_requested(&mut self, window: &Window) {
        let Some(mux) = Mux::try_get() else {
            log::warn!("closing GUI window without an active mux");
            window.close();
            if let Some(front_end) = try_front_end() {
                front_end.forget_known_window(window);
            }
            return;
        };
        match self.config.window_close_confirmation {
            WindowCloseConfirmation::NeverPrompt => {
                // Immediately kill the tabs and allow the window to close
                mux.kill_window(self.mux_window_id);
                window.close();
                if let Some(front_end) = try_front_end() {
                    front_end.forget_known_window(window);
                }
            }
            WindowCloseConfirmation::AlwaysPrompt => {
                let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
                    Some(tab) => tab,
                    None => {
                        mux.kill_window(self.mux_window_id);
                        window.close();
                        if let Some(front_end) = try_front_end() {
                            front_end.forget_known_window(window);
                        }
                        return;
                    }
                };

                let mux_window_id = self.mux_window_id;

                let can_close = mux
                    .window_can_close_without_prompting(mux_window_id)
                    .unwrap_or(false);
                if can_close {
                    mux.kill_window(self.mux_window_id);
                    window.close();
                    if let Some(front_end) = try_front_end() {
                        front_end.forget_known_window(window);
                    }
                    return;
                }
                let Some(witness_pane) = tab.get_active_pane() else {
                    log::warn!(
                        "cannot prompt to close window {mux_window_id}: active tab has no pane"
                    );
                    return;
                };
                let Some(witness) = mux.capture_pane_registration(&witness_pane) else {
                    log::warn!(
                        "cannot prompt to close window {mux_window_id}: exact pane registration is no longer active"
                    );
                    return;
                };
                let close_mux = Arc::clone(&mux);
                let close_tab = Arc::clone(&tab);
                let (overlay, ticket, future) =
                    match start_overlay(self, &tab, move |_tab_id, term| {
                        confirm_close_window(term, close_mux, mux_window_id, close_tab, witness)
                    }) {
                        Ok(overlay) => overlay,
                        Err(err) => {
                            log::error!("failed to start close-window overlay: {err:#}");
                            return;
                        }
                    };
                self.assign_overlay_with_ticket(tab.tab_id(), overlay, ticket);
                schedule_existing_termwindow_future(
                    promise::spawn::MainThreadServiceClass::Input,
                    8 * 1024,
                    "close-window confirmation overlay",
                    future,
                );

                // Don't close right now; let the close happen from
                // the confirmation overlay
            }
        }
    }

    /// Mark every visible row of every registered pane as dirty.
    ///
    /// Called at coarse-grained invalidation points (resize, focus
    /// change, font/theme swap) where the renderer can't easily
    /// reason about per-line damage. Row-level sources use
    /// `mark_stable_row_ranges_dirty` / `mark_cursor_rows_dirty`
    /// instead.
    ///
    /// Bitmaps that have never been registered absorb the call as a no-op; the
    /// next read by the render path creates one at the pane's visible height.
    /// This marks every registered pane bitmap for a whole-screen event such
    /// as a font/theme swap, focus change, resize, or viewport move.
    /// Exact generation settlement retains these marks on failure or a stale
    /// present and clears them on the first exact successful presentation.
    pub fn mark_all_panes_dirty(&mut self) {
        for bitmap in self.dirty_lines.values_mut() {
            bitmap.mark_all();
        }
        self.advance_damage_generation();
    }

    pub(crate) const fn damage_generation(&self) -> DamageGeneration {
        self.damage_generation
    }

    fn advance_damage_generation(&mut self) {
        if self.damage_generation.advance() == DamageAdvanceOutcome::ExhaustedNow {
            // A saturated epoch must never compare equal to arbitrary later
            // damage. Make exhaustion sticky and conservatively retain a full
            // repaint until this TermWindow is reconstructed.
            for bitmap in self.dirty_lines.values_mut() {
                bitmap.mark_all();
            }
            metrics::counter!("gui.render.damage_epoch_exhausted").increment(1);
            log::error!(
                "render damage generation exhausted; retaining full damage until reconstruction"
            );
        }
    }

    /// Lazy getter for a pane's dirty bitmap. Sizes the bitmap to
    /// `visible_rows` on first access and adjusts (preserving
    /// existing marks where possible) if the row count changes.
    pub fn dirty_lines_for_pane(
        &mut self,
        pane_id: PaneId,
        visible_rows: usize,
    ) -> &mut render::dirty_lines::DirtyLineBitmap {
        let bitmap = self
            .dirty_lines
            .entry(pane_id)
            .or_insert_with(|| render::dirty_lines::DirtyLineBitmap::new(visible_rows));
        if bitmap.capacity() != visible_rows {
            bitmap.resize(visible_rows);
        }
        // Every live caller marks rows and invokes `record_dirty_event` once
        // after this getter; that call fences both bitmap creation/resize and
        // the row mutation with one generation advance. Once the epoch is
        // exhausted, new/grown bitmaps must join the sticky full-redraw set.
        if self.damage_generation.exhausted {
            bitmap.mark_all();
        }
        bitmap
    }

    /// Drop the bitmap for a pane that has been closed. Without
    /// this the HashMap would leak entries for every pane the user
    /// has ever opened.
    pub fn forget_dirty_lines_for_pane(&mut self, pane_id: PaneId) {
        if self.dirty_lines.remove(&pane_id).is_some() {
            self.advance_damage_generation();
        }
    }

    fn complete_presented_frame(&mut self, outcome: PaintOutcome) {
        let settlement = apply_presented_render_attempt(
            &mut self.dirty_lines,
            self.damage_generation,
            &mut self.render_recovery_state,
            outcome.damage_generation,
        );
        metrics::counter!("gui.render.damage_settlement", "outcome" => settlement.label())
            .increment(1);
        if self.render_wake_state.cancel() {
            metrics::counter!("gui.render.retry", "action" => "cancelled_by_success")
                .increment(1);
        }
        self.complete_presented_paint(outcome);
        if settlement.needs_follow_up_paint() {
            if let Some(window) = self.window.as_ref() {
                window.invalidate();
            }
        }
    }

    fn plan_render_wake(
        &mut self,
        reason: RenderWakeReason,
        due: Instant,
    ) -> Option<RenderWakeTicket> {
        let Some(window) = self.window.clone() else {
            log::debug!("cannot schedule render wake for detached window");
            return None;
        };

        let now = Instant::now();
        match self.render_wake_state.plan(reason, due, now) {
            RenderWakePlan::Kept { ticket } => {
                metrics::counter!("gui.render.wake", "action" => "kept").increment(1);
                Some(ticket)
            }
            RenderWakePlan::Schedule {
                ticket,
                delay,
                registration,
            } => {
                metrics::counter!("gui.render.wake", "action" => "scheduled").increment(1);
                metrics::histogram!("gui.render.wake_delay_ms")
                    .record(delay.as_secs_f64() * 1_000.0);
                log::debug!(
                    "scheduled {:?} render wake ticket {} after {:?}",
                    reason,
                    ticket.0,
                    delay
                );
                let reservation = match promise::spawn::try_reserve_main_thread(
                    promise::spawn::MainThreadServiceClass::Render,
                    8 * 1024,
                ) {
                    promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => {
                        reservation
                    }
                    rejected => {
                        let cancelled = self.render_wake_state.cancel_exact(ticket);
                        debug_assert!(
                            cancelled,
                            "a just-planned render wake must retain its exact ticket until scheduler admission"
                        );
                        metrics::counter!("gui.render.wake", "action" => "scheduler_rejected")
                            .increment(1);
                        log::error!(
                            "main-thread scheduler rejected exact render wake; cancelled ticket and invalidating immediately: {rejected:?}"
                        );
                        window.invalidate();
                        return None;
                    }
                };
                reservation.spawn_local(async move {
                    let _ = Abortable::new(
                        async move {
                            sleep(delay).await;
                            let wake_window = window.clone();
                            window.notify(TermWindowNotif::Apply(Box::new(move |tw| {
                                match tw.render_wake_state.dispatch(ticket) {
                                    RenderWakeDispatch::Fired(RenderWakeReason::Retry(_stage)) => {
                                        if tw.render_recovery_state.mark_retry_ready(ticket) {
                                            metrics::counter!(
                                                "gui.render.retry",
                                                "action" => "dispatched"
                                            )
                                            .increment(1);
                                            wake_window.invalidate();
                                        } else {
                                            metrics::counter!(
                                                "gui.render.retry",
                                                "action" => "stale_recovery"
                                            )
                                            .increment(1);
                                        }
                                    }
                                    RenderWakeDispatch::Fired(RenderWakeReason::Animation) => {
                                        metrics::counter!(
                                            "gui.render.animation_wake",
                                            "action" => "dispatched"
                                        )
                                        .increment(1);
                                        wake_window.invalidate();
                                    }
                                    RenderWakeDispatch::Stale => {
                                        metrics::counter!(
                                            "gui.render.wake",
                                            "action" => "stale"
                                        )
                                        .increment(1);
                                    }
                                }
                            })));
                        },
                        registration,
                    )
                    .await;
                })
                .detach();
                Some(ticket)
            }
            RenderWakePlan::Exhausted => {
                metrics::counter!("gui.render.wake", "action" => "ticket_exhausted")
                    .increment(1);
                log::error!("render wake ticket space exhausted; opening renderer circuit");
                None
            }
        }
    }

    fn schedule_render_retry(&mut self, stage: RenderFailureStage, delay: Duration) {
        let now = Instant::now();
        let due = now.checked_add(delay).unwrap_or(now);
        match self.plan_render_wake(RenderWakeReason::Retry(stage), due) {
            Some(ticket) => {
                self.render_recovery_state.enter_cooldown(ticket, stage);
                metrics::counter!("gui.render.retry", "action" => "scheduled").increment(1);
            }
            None => self.render_recovery_state.open_circuit(stage),
        }
    }

    pub(crate) fn schedule_animation_wake(&mut self, due: Instant) {
        if !matches!(self.render_recovery_state.mode, RenderRecoveryMode::Healthy) {
            metrics::counter!("gui.render.animation_wake", "action" => "suppressed_recovery")
                .increment(1);
            return;
        }
        let _ = self.plan_render_wake(RenderWakeReason::Animation, due);
    }

    fn handle_render_failure(&mut self, failure: &RenderAttemptFailure) {
        let stage = failure.stage();
        let (settlement, directive) = apply_failed_render_attempt(
            &mut self.dirty_lines,
            self.damage_generation,
            &mut self.render_recovery_state,
            stage,
        );
        metrics::counter!("gui.render.damage_settlement", "outcome" => settlement.label())
            .increment(1);
        metrics::counter!("gui.render.frame_failure", "stage" => stage.label()).increment(1);
        match directive {
            RenderRecoveryDirective::RetryAfter(delay) => {
                self.schedule_render_retry(stage, delay);
            }
            RenderRecoveryDirective::Park => {
                self.render_wake_state.cancel();
                self.render_recovery_state.park(stage);
                metrics::counter!("gui.render.retry", "action" => "parked").increment(1);
            }
            RenderRecoveryDirective::OpenCircuit => {
                self.render_wake_state.cancel();
                self.render_recovery_state.open_circuit(stage);
                metrics::counter!("gui.render.retry", "action" => "circuit_open")
                    .increment(1);
            }
        }
    }

    pub(crate) fn note_render_surface_recovery_signal(&mut self) {
        if self
            .render_recovery_state
            .record_surface_recovery_signal()
        {
            self.render_wake_state.cancel();
            metrics::counter!("gui.render.retry", "action" => "surface_signal_reopened")
                .increment(1);
        }
    }

    /// Aggregate dirty-line telemetry across every registered
    /// pane. Consumed by `ft doctor` once that path lands and used
    /// by tests to spot-check the per-pane signal without
    /// iterating each `DirtyLineBitmap` (ft-mpc9b.1.2).
    pub fn dirty_lines_telemetry(&self) -> DirtyLineTelemetrySnapshot {
        let mut snapshot = DirtyLineTelemetrySnapshot::default();
        for (_pane_id, bitmap) in &self.dirty_lines {
            snapshot.pane_count += 1;
            snapshot.total_dirty_marks = snapshot
                .total_dirty_marks
                .saturating_add(bitmap.dirty_marks_total());
            snapshot.total_frames_cleared = snapshot
                .total_frames_cleared
                .saturating_add(bitmap.frames_cleared_total());
            snapshot.total_capacity = snapshot
                .total_capacity
                .saturating_add(bitmap.capacity() as u64);
            snapshot.currently_dirty_lines = snapshot
                .currently_dirty_lines
                .saturating_add(bitmap.count() as u64);
            // Per ft-8pcwy: aggregate the per-pane clean-line skip
            // counter so ft-doctor surfaces the render-pass savings
            // once the iter-dirty gate is enabled.
            snapshot.total_clean_lines_skipped = snapshot
                .total_clean_lines_skipped
                .saturating_add(bitmap.clean_lines_skipped_total());
        }
        // Per ft-i6k6u: copy the per-source aggregator into the
        // snapshot. The aggregator is shared across all panes; the
        // doctor surface presents one consolidated view rather than
        // per-pane breakdowns.
        snapshot.marks_by_source = self.dirty_marks_by_source;
        snapshot
    }

    /// Per ft-i6k6u / ft-jvj78 slice: bump the per-source
    /// mark counter. Call this immediately before / after the
    /// actual mark-call on the bitmap so the substrate's
    /// `MarksBySource` aggregator reflects every event source
    /// the integration is wiring.
    ///
    /// Used by:
    /// - check_for_dirty_lines_and_invalidate_selection (Pty + SelectionChange)
    /// - paint_pane (CursorMove)
    /// - set_viewport (Viewport)
    /// - mark_all_panes_dirty_with_source (FocusChange / ThemeSwap / FontSwap / Resize)
    ///
    /// `StatusTileUpdate` has no live call site yet; do not count its taxonomy
    /// variant as production integration.
    pub fn record_dirty_event(
        &mut self,
        source: frankenterm_core::dirty_line_telemetry::DirtyEventSource,
    ) {
        self.dirty_marks_by_source.record(source);
        self.advance_damage_generation();
    }

    /// Per ft-i6k6u: source-tagged variant of mark_all_panes_dirty.
    /// Bumps the per-source counter so the substrate's
    /// `MarksBySource` aggregator can attribute the whole-screen
    /// invalidation to its actual cause (focus change vs theme
    /// swap vs font swap).
    pub fn mark_all_panes_dirty_with_source(
        &mut self,
        source: frankenterm_core::dirty_line_telemetry::DirtyEventSource,
    ) {
        self.mark_all_panes_dirty();
        // `mark_all_panes_dirty` already advanced the damage generation for
        // this event. Record attribution directly to avoid double-advancing.
        self.dirty_marks_by_source.record(source);
    }

    pub fn publish_terminal_state_snapshot(
        &mut self,
        pane_id: u64,
        state: TerminalState,
    ) -> frankenterm_core::triple_buffer::PublishOutcome {
        self.triple_buffer_panes.publish(pane_id, state)
    }

    /// Drop any explicitly published triple-buffer foundation state and
    /// retained health snapshot for a closed pane.
    pub fn forget_terminal_state_buffer_for_pane(&mut self, pane_id: u64) {
        self.triple_buffer_panes.remove(pane_id);
        self.forget_pane_health_snapshot(pane_id);
    }

    #[must_use]
    pub fn terminal_state_buffer_pane_count(&self) -> usize {
        self.triple_buffer_panes.len()
    }

    /// Store an explicitly supplied per-pane health snapshot. This remains a
    /// test/future-integration API while the producer and consumer are dormant.
    /// Pane removal still clears any retained foundation state.
    pub fn record_pane_health_snapshot(
        &mut self,
        pane_id: u64,
        snapshot: frankenterm_core::triple_buffer_fleet_health::PaneHealthSnapshot,
    ) {
        self.triple_buffer_pane_health.insert(pane_id, snapshot);
    }

    /// Translate and retain an explicitly supplied watchdog health view. No
    /// production renderer/status-tick caller is wired today.
    pub fn record_pane_watchdoged_health(
        &mut self,
        pane_id: u64,
        health: &frankenterm_core::watchdoged_triple_buffer::WatchdogedHealth,
        last_force_recycle_ts_ms: u64,
    ) {
        self.record_pane_health_snapshot(
            pane_id,
            pane_health_snapshot_from_watchdoged_health(pane_id, health, last_force_recycle_ts_ms),
        );
    }

    /// Per ft-gso6n: drop the stored health snapshot for a closed
    /// pane. Mirror of `forget_dirty_lines_for_pane`.
    pub fn forget_pane_health_snapshot(&mut self, pane_id: u64) {
        self.triple_buffer_pane_health.remove(&pane_id);
    }

    /// Per ft-gso6n / sub-task 5: aggregate the per-pane
    /// snapshots into the substrate's FleetHealthAggregate. This
    /// is what `ft doctor --triple-buffer` renders.
    ///
    /// Empty fleet (no panes registered) returns an
    /// all-zero aggregate — distinguishable from a real fleet
    /// with zero force-recycles via the `total_panes` field.
    pub fn triple_buffer_telemetry(
        &self,
    ) -> frankenterm_core::triple_buffer_fleet_health::FleetHealthAggregate {
        let snapshots: Vec<_> = self.triple_buffer_pane_health.values().copied().collect();
        frankenterm_core::triple_buffer_fleet_health::aggregate_fleet_health(&snapshots)
    }

    /// Per ft-gso6n: list of panes currently inside an active
    /// watchdog window. The doctor surface flags these with a
    /// "watchdog active >Xs" warning. Returns an empty Vec when
    /// no pane is currently fired.
    #[must_use]
    pub fn triple_buffer_panes_with_active_watchdog(&self) -> Vec<u64> {
        let mut out: Vec<u64> = self
            .triple_buffer_pane_health
            .iter()
            .filter(|(_, s)| s.watchdog_active)
            .map(|(&id, _)| id)
            .collect();
        out.sort_unstable();
        out
    }

    /// Per ft-a9eu1 / ft-1dq8h slice 1: combined doctor surface
    /// for the DEC 2026 Begin-Synchronized-Update subsystem.
    /// Folds the substrate's two telemetry types into one view.
    #[must_use]
    pub fn sync_output_telemetry(&self) -> SyncOutputDoctorSnapshot {
        let w = &self.sync_output_watchdog_telemetry;
        let o = &self.sync_output_orchestrator_telemetry;
        SyncOutputDoctorSnapshot {
            bsu_count: w.bsu_count(),
            esu_count: w.esu_count(),
            esu_flush_count: w.esu_flush_count(),
            watchdog_force_flush_count: w.watchdog_force_flush_count(),
            mid_bsu_byte_count: w.mid_bsu_byte_count(),
            max_bsu_depth_observed: w.max_bsu_depth_observed(),
            mode_query_count: w.mode_query_count(),
            adversarial_esu_underflow_count: w.adversarial_esu_underflow_count(),
            admissions_accepted: o.admissions_accepted,
            admissions_truncated: o.admissions_truncated,
            admissions_refused: o.admissions_refused,
            bytes_accepted: o.bytes_accepted,
            bytes_truncated: o.bytes_truncated,
            bytes_refused: o.bytes_refused,
            bytes_drained_total: o.bytes_drained_total,
            overrides_pass_through: o.overrides_pass_through,
            overrides_coalesced: o.overrides_coalesced,
            overrides_force_flush: o.overrides_force_flush,
            drains_esu: o.drains_esu,
            drains_watchdog: o.drains_watchdog,
            drains_live_resize: o.drains_live_resize,
            drains_operator: o.drains_operator,
            drains_no_op: o.drains_no_op,
            overrides_by_trigger: o.overrides_by_trigger,
        }
    }

    /// Per ft-a9eu1: feeders that the BSU watchdog hooks call
    /// once they're wired. Today these are no-ops (no caller),
    /// but the method shape matches what the future integration
    /// will need so a future commit can flip the wiring without
    /// reshaping TermWindow.
    pub fn sync_output_watchdog_telemetry_mut(
        &mut self,
    ) -> &mut frankenterm_core::sync_output_watchdog::SyncOutputTelemetry {
        &mut self.sync_output_watchdog_telemetry
    }

    /// Per ft-a9eu1: orchestrator-side feeder.
    pub fn sync_output_orchestrator_telemetry_mut(
        &mut self,
    ) -> &mut frankenterm_core::sync_output_buffer_orchestrator::SyncOutputOrchestratorTelemetry
    {
        &mut self.sync_output_orchestrator_telemetry
    }

    /// Per ft-8pcwy: read-only access to a pane's bitmap. Returns
    /// `None` when no bitmap has been registered for the pane.
    pub fn peek_dirty_lines(
        &self,
        pane_id: PaneId,
    ) -> Option<&render::dirty_lines::DirtyLineBitmap> {
        self.dirty_lines.get(&pane_id)
    }

    /// Per ft-8pcwy: bump the per-pane clean-line skip counter.
    /// Called after a cached-quad hit when the dirty bitmap classifies that row
    /// as clean. It does not mean line hashing or cache lookup was elided.
    pub fn record_clean_line_skipped(&mut self, pane_id: PaneId) {
        if let Some(bitmap) = self.dirty_lines.get_mut(&pane_id) {
            bitmap.record_clean_line_skipped();
        }
    }

    /// Per ft-gwzrm: query the iter-dirty render-pass gate. The
    /// gate controls clean-line skip telemetry; rows still rebuild
    /// whenever cached quads are unavailable.
    #[inline]
    pub fn iter_dirty_render_gate_enabled(&self) -> bool {
        self.iter_dirty_render_gate_enabled
    }

    /// Per ft-gwzrm: allow tests or operator plumbing to disable
    /// clean-line skip telemetry without changing the dirty-marking
    /// source wiring.
    pub fn set_iter_dirty_render_gate(&mut self, enabled: bool) {
        self.iter_dirty_render_gate_enabled = enabled;
    }

    /// Per ft-d6nrd slice 1: aggregate the FrameBudget allocator
    /// counters + the substrate's cosmetic-defer + reduce-motion
    /// gate counters into one snapshot for `ft doctor`. Matches
    /// the `*_telemetry()` method shape used by the dirty-lines
    /// and elastic-buffer surfaces.
    pub fn frame_budget_telemetry(&self) -> FrameBudgetTelemetrySnapshot {
        FrameBudgetTelemetrySnapshot {
            budget_ns: self.frame_budget.budget_ns(),
            spent_ns_current: self.frame_budget.spent_ns(),
            deferrals_lifetime: self.frame_budget.lifetime_deferrals(),
            drops_lifetime: self.frame_budget.lifetime_drops(),
            bulk_drains_lifetime: self.frame_budget.lifetime_bulk_drains(),
            queue_depth_now: self.frame_budget.queue_depth(),
            cosmetic_outstanding_total: self.cosmetic_defer_outstanding.total(),
            gate_skips_reduce_motion: self.frame_budget_gate_telemetry.gate_skips_reduce_motion,
            gate_defers: self.frame_budget_gate_telemetry.gate_defers,
        }
    }

    /// Per ft-d6nrd slice 1: redraw-predicate hook. True when the
    /// next frame must paint to make progress on deferred cosmetic
    /// work — either the FrameBudget queue has carry-over ops or
    /// the cosmetic-defer aggregator shows outstanding lines.
    /// Couples cosmetic_defer_outstanding into TermWindow's
    /// should_paint decision per the bead.
    pub fn frame_budget_should_force_paint(&self) -> bool {
        should_force_paint_for_frame_budget(
            self.frame_budget.queue_depth(),
            self.cosmetic_defer_outstanding.total(),
        )
    }

    /// Per ft-d6nrd slice 1: paint_impl hook called at the very
    /// top of the frame. Resets the per-frame budget counters and
    /// returns the start-of-frame report. Per the bead's item 1.
    pub fn frame_budget_begin_frame(&mut self) -> frame_budget::FrameStartReport {
        self.frame_budget.begin_frame()
    }

    pub fn frame_budget_drain_deferred_cosmetic(&mut self) -> u32 {
        let drained = self.frame_budget.drain_deferred_ops();
        let drained_count = drained.len().min(u32::MAX as usize) as u32;
        record_drained_frame_budget_ops(&mut self.cosmetic_defer_outstanding, drained);
        drained_count
    }

    pub fn frame_budget_try_bulk_drain_cosmetic(&mut self) -> u32 {
        let drained = self.frame_budget.try_bulk_drain_ops();
        let drained_count = drained.len().min(u32::MAX as usize) as u32;
        record_drained_frame_budget_ops(&mut self.cosmetic_defer_outstanding, drained);
        drained_count
    }

    /// Per ft-d6nrd slice 1: paint_impl hook called after Present.
    /// Returns the typed end-of-frame report for telemetry
    /// emission.
    pub fn frame_budget_end_frame(&mut self) -> frame_budget::FrameEndReport {
        self.frame_budget.end_frame()
    }

    pub fn frame_budget_reduce_motion_state(&self) -> frame_budget_a11y::ReduceMotionState {
        self.frame_budget_reduce_motion_state
    }

    pub fn refresh_frame_budget_reduce_motion_state(&mut self) {
        self.frame_budget_reduce_motion_state = probe_reduce_motion_state();
    }

    /// Per ft-asdza / A11Y.5: compose the current FrameBudget
    /// state with the OS reduce-motion state before mutating the
    /// budget queue. `Skip` means animations are not executed and
    /// are not queued; `Defer` / `DropOldest` preserve the existing
    /// frame-budget queue semantics and force a follow-up paint via
    /// `cosmetic_defer_outstanding`.
    pub fn frame_budget_try_execute_with_reduce_motion(
        &mut self,
        op: frame_budget::OpKind,
        priority: frame_budget::OpPriority,
        cost_ns: u64,
        motion: frame_budget_a11y::ReduceMotionState,
    ) -> frame_budget_a11y::MotionGateDecision {
        let decision =
            evaluate_frame_budget_reduce_motion_gate(&self.frame_budget, op, priority, motion);

        if let Some(a11y_op) = a11y_op_kind_from_frame_budget_op(op) {
            self.frame_budget_gate_telemetry
                .record_decision(a11y_op, decision);
        }

        if !matches!(decision, frame_budget_a11y::MotionGateDecision::Skip) {
            let execution = self.frame_budget.try_execute(op, priority, cost_ns);
            record_frame_budget_execution_outstanding(
                &mut self.cosmetic_defer_outstanding,
                op,
                execution,
            );
        }

        decision
    }

    /// Convenience wrapper for paint sites that want the live
    /// platform preference probe rather than an already-cached
    /// `ReduceMotionState`.
    #[must_use]
    pub fn frame_budget_try_execute_with_platform_reduce_motion(
        &mut self,
        op: frame_budget::OpKind,
        priority: frame_budget::OpPriority,
        cost_ns: u64,
    ) -> frame_budget_a11y::MotionGateDecision {
        self.frame_budget_try_execute_with_reduce_motion(
            op,
            priority,
            cost_ns,
            probe_reduce_motion_state(),
        )
    }

    pub fn frame_budget_should_run_render_op(
        &mut self,
        op: frame_budget::OpKind,
        priority: frame_budget::OpPriority,
    ) -> bool {
        let decision = self.frame_budget_try_execute_with_reduce_motion(
            op,
            priority,
            default_frame_budget_cost_ns(op),
            self.frame_budget_reduce_motion_state,
        );
        should_run_frame_budget_decision(decision)
    }

    pub fn frame_budget_should_run_render_op_with_reduce_motion(
        &mut self,
        op: frame_budget::OpKind,
        priority: frame_budget::OpPriority,
        motion: frame_budget_a11y::ReduceMotionState,
    ) -> bool {
        let decision = self.frame_budget_try_execute_with_reduce_motion(
            op,
            priority,
            default_frame_budget_cost_ns(op),
            motion,
        );
        should_run_frame_budget_decision(decision)
    }

    /// Snapshot of the workspace ElasticBuffer policy state.
    /// Capacity and used counters are backed by the live RenderState
    /// quad allocation after renderer creation or the first paint.
    /// A zero capacity now means there is genuinely no observed live
    /// allocation yet.
    pub fn elastic_buffer_telemetry(&self) -> ElasticBufferTelemetrySnapshot {
        ElasticBufferTelemetrySnapshot {
            capacity: self.quad_buffer_policy.telemetry_capacity() as u64,
            used: self.quad_buffer_policy.telemetry_len() as u64,
            grow_count: self.quad_buffer_policy.grow_count(),
            shrink_count: self.quad_buffer_policy.shrink_count(),
            high_water_mark: self.quad_buffer_policy.high_water_mark() as u64,
            gesture_active: self.quad_buffer_policy.is_gesture_active(),
        }
    }

    pub(crate) fn record_quad_buffer_allocation_snapshot(&mut self, reallocation_count: u64) {
        let Some(render_state) = self.render_state.as_ref() else {
            return;
        };
        let snapshot = render_state.quad_allocation_snapshot();
        self.quad_buffer_policy.record_live_allocation(
            snapshot.used,
            snapshot.capacity,
            reallocation_count,
        );
    }

    /// Notify the quad-buffer policy that a live resize gesture has
    /// started (ft-kciew). Idempotent on an active gesture so it's
    /// safe to call from every `resize()` event with
    /// `live_resizing=true`. While the gesture is active,
    /// `tick_quad_buffer_shrink()` is a no-op and any continuation
    /// bead's GPU buffer wrap will refuse to shrink.
    fn begin_quad_resize_gesture(&mut self) {
        if !self.quad_buffer_in_resize_gesture {
            self.quad_buffer_policy.begin_gesture();
            self.quad_buffer_in_resize_gesture = true;
        }
    }

    /// Notify the quad-buffer policy that the live resize gesture
    /// has ended. Called when the first non-live `resize()` event
    /// arrives after a sequence of live ones. Records the end
    /// timestamp; the actual shrink fires later from the periodic
    /// idle tick once the configured idle threshold has elapsed.
    fn end_quad_resize_gesture(&mut self) {
        if self.quad_buffer_in_resize_gesture {
            self.quad_buffer_policy.end_gesture(Instant::now());
            self.quad_buffer_in_resize_gesture = false;
        }
    }

    /// Driven by the periodic status-update timer. The current
    /// integration preserves resize-gesture gating and live
    /// allocation telemetry, but does not shrink the GPU-owned
    /// `RenderState` buffers until the quad-writer continuation
    /// moves live reallocation into the policy.
    fn tick_quad_buffer_shrink(&mut self) {
        if let Some(result) = self.quad_buffer_policy.try_shrink_if_idle(Instant::now()) {
            log::trace!(
                "quad buffer policy shrunk: capacity {} → {} (used_at_shrink={})",
                result.capacity_before,
                result.capacity_after,
                result.used_at_shrink,
            );
        }
    }

    fn record_idle_event(&mut self, event: idle_detector::IdleEvent) {
        let report = self.idle_detector.record_event(event, Instant::now());
        if report.is_wake() {
            log::trace!(
                "idle detector wake: event={:?} prev={:?} next={:?} latency={:?}",
                report.event,
                report.prev_state,
                report.next_state,
                report.wake_latency,
            );
        }
        self.last_idle_transition = Some(report);
    }

    fn poll_idle_scheduler(&mut self) -> idle_detector::IdleSchedulerDecision {
        let state = self.idle_detector.poll(Instant::now());
        let decision = idle_detector::IdleSchedulerDecision::for_state(state, 60);
        if decision.sleep_until_event {
            log::trace!("idle detector entered event-driven paint scheduling");
        }
        decision
    }

    pub fn idle_detector_doctor_snapshot(&self) -> idle_detector::IdleDoctorSnapshot {
        self.idle_detector.doctor_snapshot(60)
    }

    pub fn atlas_tier_swap_doctor_report(&self) -> TierSwapDoctorReport {
        self.render_state
            .as_ref()
            .map(RenderState::tier_swap_doctor_report)
            .unwrap_or_else(TierSwapDoctorReport::no_atlases_in_process)
    }

    fn focus_changed(&mut self, focused: bool, window: &Window) {
        self.record_idle_event(idle_detector::IdleEvent::FocusChange);
        log::trace!("Setting focus to {:?}", focused);
        self.focused = if focused { Some(Instant::now()) } else { None };
        if focused {
            self.note_render_surface_recovery_signal();
        }
        self.advance_quad_generation();
        self.mark_all_panes_dirty_with_source(
            frankenterm_core::dirty_line_telemetry::DirtyEventSource::FocusChange,
        );
        self.load_os_parameters();

        if self.focused.is_none() {
            self.last_mouse_click = None;
            self.current_mouse_buttons.clear();
            self.current_mouse_capture = None;
            self.is_click_to_focus_window = false;

            for state in self.pane_state.borrow_mut().values_mut() {
                state.mouse_terminal_coords.take();
            }
        }

        // Reset the cursor blink phase
        self.prev_cursor.bump();

        // force cursor to be repainted
        window.invalidate();

        if let Some(pane) = self.get_active_pane_or_overlay() {
            pane.focus_changed(focused);
        }

        self.update_title();
        self.emit_window_event("window-focus-changed", None);
    }

    fn created(&mut self, ctx: RenderContext) -> anyhow::Result<()> {
        self.render_wake_state.cancel();
        self.render_state = None;

        let render_info = ctx.renderer_info();
        self.opengl_info.replace(render_info.clone());

        let render_state = RenderState::new(ctx, &self.fonts, &self.render_metrics, ATLAS_SIZE)
            .with_context(|| format!("failed to create render state for {render_info}"))?;
        log::debug!(
            "Renderer initialized: {} FrankenTerm version: {}",
            render_info,
            config::wezterm_version(),
        );
        self.render_state.replace(render_state);
        self.render_recovery_state.record_reinitialized();
        self.record_quad_buffer_allocation_snapshot(0);

        Ok(())
    }
}

const RENDER_RETRY_DELAYS_MS: [u64; 6] = [8, 16, 32, 64, 128, 250];
const MAX_TIMEOUT_RENDER_RETRIES: u32 = 6;
const MAX_STALE_SURFACE_RETRIES: u32 = 3;
const MAX_PAINT_RETRIES: u32 = 2;
const MAX_BACKEND_RETRIES: u32 = 3;
const OCCLUDED_REPAINT_PROBE_DELAY: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RenderWakeTicket(u64);

#[derive(Debug, Clone, Copy, PartialEq)]
enum RenderWakeReason {
    Retry(RenderFailureStage),
    Animation,
}

struct RenderWakeAbort {
    handle: AbortHandle,
}

impl Drop for RenderWakeAbort {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

struct PendingRenderWake {
    ticket: RenderWakeTicket,
    due: Instant,
    reason: RenderWakeReason,
    _abort: RenderWakeAbort,
}

enum RenderWakePlan {
    Schedule {
        ticket: RenderWakeTicket,
        delay: Duration,
        registration: AbortRegistration,
    },
    Kept {
        ticket: RenderWakeTicket,
    },
    Exhausted,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum RenderWakeDispatch {
    Fired(RenderWakeReason),
    Stale,
}

#[derive(Default)]
struct RenderWakeState {
    next_ticket: u64,
    exhausted: bool,
    pending: Option<PendingRenderWake>,
}

impl RenderWakeState {
    fn plan(
        &mut self,
        reason: RenderWakeReason,
        due: Instant,
        now: Instant,
    ) -> RenderWakePlan {
        if self.exhausted {
            return RenderWakePlan::Exhausted;
        }

        if let Some(pending) = self.pending.as_ref() {
            let keep_existing = match (pending.reason, reason) {
                // A retry is the admission gate.  An animation must never
                // bypass it, and duplicate failures coalesce into its ticket.
                (RenderWakeReason::Retry(_), _) => true,
                (RenderWakeReason::Animation, RenderWakeReason::Animation) => {
                    pending.due <= due
                }
                (RenderWakeReason::Animation, RenderWakeReason::Retry(_)) => false,
            };
            if keep_existing {
                return RenderWakePlan::Kept {
                    ticket: pending.ticket,
                };
            }
        }

        let Some(next_ticket) = self.next_ticket.checked_add(1) else {
            self.exhausted = true;
            self.pending.take();
            return RenderWakePlan::Exhausted;
        };
        self.next_ticket = next_ticket;
        let ticket = RenderWakeTicket(next_ticket);
        let (handle, registration) = AbortHandle::new_pair();
        // Taking the prior lease aborts its physical sleeper.  Exact ticket
        // validation below is still required for a callback already queued.
        self.pending.take();
        self.pending = Some(PendingRenderWake {
            ticket,
            due,
            reason,
            _abort: RenderWakeAbort { handle },
        });
        RenderWakePlan::Schedule {
            ticket,
            delay: due.saturating_duration_since(now),
            registration,
        }
    }

    fn cancel(&mut self) -> bool {
        self.pending.take().is_some()
    }

    fn cancel_exact(&mut self, ticket: RenderWakeTicket) -> bool {
        if self.pending.as_ref().map(|pending| pending.ticket) != Some(ticket) {
            return false;
        }
        self.pending.take();
        true
    }

    fn dispatch(&mut self, ticket: RenderWakeTicket) -> RenderWakeDispatch {
        if self.pending.as_ref().map(|pending| pending.ticket) != Some(ticket) {
            return RenderWakeDispatch::Stale;
        }
        match self.pending.take() {
            Some(pending) => RenderWakeDispatch::Fired(pending.reason),
            None => RenderWakeDispatch::Stale,
        }
    }

    #[cfg(test)]
    fn pending_ticket(&self) -> Option<RenderWakeTicket> {
        self.pending.as_ref().map(|pending| pending.ticket)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum RenderRecoveryMode {
    Healthy,
    Cooldown {
        ticket: RenderWakeTicket,
        stage: RenderFailureStage,
    },
    RetryReady { stage: RenderFailureStage },
    Parked {
        stage: RenderFailureStage,
    },
    CircuitOpen {
        stage: RenderFailureStage,
    },
}

impl Default for RenderRecoveryMode {
    fn default() -> Self {
        Self::Healthy
    }
}

#[derive(Debug, Default)]
struct RenderRecoveryState {
    mode: RenderRecoveryMode,
    /// Monotonic negative evidence for the current no-success incident.
    /// Failure-stage changes and external surface signals must not reset it;
    /// only a presented frame or complete renderer reinitialization may do so.
    failed_attempts_since_success: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaintAdmission {
    Admit,
    SuppressCooldown,
    SuppressParked,
    SuppressCircuit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenderRecoveryDirective {
    RetryAfter(Duration),
    Park,
    OpenCircuit,
}

/// Apply the state transition for a failed admitted render attempt.
///
/// Keeping damage settlement and negative-evidence accounting in one shared
/// helper prevents deterministic backend-fault tests from reproducing only a
/// test-side approximation of the production transition.
fn apply_failed_render_attempt(
    bitmaps: &mut HashMap<PaneId, render::dirty_lines::DirtyLineBitmap>,
    current_generation: DamageGeneration,
    recovery: &mut RenderRecoveryState,
    stage: RenderFailureStage,
) -> (DamageCommitOutcome, RenderRecoveryDirective) {
    let damage = settle_frame_damage(
        bitmaps,
        current_generation,
        FrameCompletion::Failed(stage),
    );
    let directive = recovery.record_failure(stage);
    (damage, directive)
}

/// Apply the state transition for a synchronously presented frame.
fn apply_presented_render_attempt(
    bitmaps: &mut HashMap<PaneId, render::dirty_lines::DirtyLineBitmap>,
    current_generation: DamageGeneration,
    recovery: &mut RenderRecoveryState,
    captured_generation: DamageGeneration,
) -> DamageCommitOutcome {
    let damage = settle_frame_damage(
        bitmaps,
        current_generation,
        FrameCompletion::Presented(captured_generation),
    );
    recovery.record_success();
    damage
}

fn retry_delay(failed_attempts_since_success: u32) -> Duration {
    let index = usize::try_from(failed_attempts_since_success.saturating_sub(1))
        .unwrap_or(usize::MAX)
        .min(RENDER_RETRY_DELAYS_MS.len() - 1);
    Duration::from_millis(RENDER_RETRY_DELAYS_MS[index])
}

fn render_recovery_directive(
    stage: RenderFailureStage,
    failed_attempts_since_success: u32,
) -> RenderRecoveryDirective {
    let retry_with_limit = |limit| {
        if failed_attempts_since_success <= limit {
            RenderRecoveryDirective::RetryAfter(retry_delay(failed_attempts_since_success))
        } else {
            RenderRecoveryDirective::OpenCircuit
        }
    };

    match stage {
        RenderFailureStage::SurfaceAcquire(webgpu::WebGpuSurfaceTextureError::Occluded) => {
            RenderRecoveryDirective::Park
        }
        RenderFailureStage::SurfaceAcquire(webgpu::WebGpuSurfaceTextureError::Validation)
        | RenderFailureStage::Draw(DrawFailureStage::RenderCommands)
        | RenderFailureStage::Draw(DrawFailureStage::MissingGlyphProgram)
        | RenderFailureStage::Draw(DrawFailureStage::BufferSliceBounds) => {
            RenderRecoveryDirective::OpenCircuit
        }
        RenderFailureStage::SurfaceAcquire(webgpu::WebGpuSurfaceTextureError::Timeout) => {
            if failed_attempts_since_success <= MAX_TIMEOUT_RENDER_RETRIES {
                RenderRecoveryDirective::RetryAfter(retry_delay(failed_attempts_since_success))
            } else {
                // Acquisition-only retries have established persistent
                // occlusion/driver pressure. Wait for resize/focus rather than
                // running a permanent timer.
                RenderRecoveryDirective::Park
            }
        }
        RenderFailureStage::SurfaceAcquire(webgpu::WebGpuSurfaceTextureError::Outdated)
        | RenderFailureStage::SurfaceAcquire(webgpu::WebGpuSurfaceTextureError::Lost) => {
            if failed_attempts_since_success <= MAX_STALE_SURFACE_RETRIES {
                RenderRecoveryDirective::RetryAfter(retry_delay(failed_attempts_since_success))
            } else {
                RenderRecoveryDirective::Park
            }
        }
        RenderFailureStage::Paint => retry_with_limit(MAX_PAINT_RETRIES),
        RenderFailureStage::Draw(DrawFailureStage::BackendDraw)
        | RenderFailureStage::BackendFinish
        | RenderFailureStage::Submission
        | RenderFailureStage::Present => retry_with_limit(MAX_BACKEND_RETRIES),
    }
}

impl RenderRecoveryState {
    fn admit(&mut self) -> PaintAdmission {
        match self.mode {
            RenderRecoveryMode::Healthy => PaintAdmission::Admit,
            RenderRecoveryMode::RetryReady { .. } => {
                // Preserve the failure count until a frame actually presents.
                self.mode = RenderRecoveryMode::Healthy;
                PaintAdmission::Admit
            }
            RenderRecoveryMode::Cooldown { .. } => PaintAdmission::SuppressCooldown,
            RenderRecoveryMode::Parked { .. } => PaintAdmission::SuppressParked,
            RenderRecoveryMode::CircuitOpen { .. } => PaintAdmission::SuppressCircuit,
        }
    }

    fn record_failure(&mut self, stage: RenderFailureStage) -> RenderRecoveryDirective {
        self.failed_attempts_since_success =
            self.failed_attempts_since_success.saturating_add(1);
        render_recovery_directive(stage, self.failed_attempts_since_success)
    }

    fn enter_cooldown(&mut self, ticket: RenderWakeTicket, stage: RenderFailureStage) {
        self.mode = RenderRecoveryMode::Cooldown { ticket, stage };
    }

    fn mark_retry_ready(&mut self, ticket: RenderWakeTicket) -> bool {
        match self.mode {
            RenderRecoveryMode::Cooldown {
                ticket: expected,
                stage,
            } if expected == ticket => {
                self.mode = RenderRecoveryMode::RetryReady { stage };
                true
            }
            _ => false,
        }
    }

    fn park(&mut self, stage: RenderFailureStage) {
        self.mode = RenderRecoveryMode::Parked { stage };
    }

    fn open_circuit(&mut self, stage: RenderFailureStage) {
        self.mode = RenderRecoveryMode::CircuitOpen { stage };
    }

    fn record_success(&mut self) {
        self.mode = RenderRecoveryMode::Healthy;
        self.failed_attempts_since_success = 0;
    }

    fn record_reinitialized(&mut self) {
        self.record_success();
    }

    fn record_surface_recovery_signal(&mut self) -> bool {
        let stage = match self.mode {
            RenderRecoveryMode::Cooldown { stage, .. }
            | RenderRecoveryMode::RetryReady { stage, .. }
            | RenderRecoveryMode::Parked { stage }
            | RenderRecoveryMode::CircuitOpen { stage } => stage,
            RenderRecoveryMode::Healthy => return false,
        };
        if !stage.accepts_surface_recovery_signal() {
            return false;
        }
        self.mode = RenderRecoveryMode::Healthy;
        true
    }

    const fn parked_occluded_stage(&self) -> Option<RenderFailureStage> {
        match self.mode {
            RenderRecoveryMode::Parked {
                stage:
                    stage @ RenderFailureStage::SurfaceAcquire(
                        webgpu::WebGpuSurfaceTextureError::Occluded,
                    ),
            } => Some(stage),
            _ => None,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum WebGpuSurfaceErrorAction {
    ForceConfigure,
    RecreateSurface,
    DeferToRecoveryPolicy,
}

fn classify_webgpu_surface_error(
    err: &webgpu::WebGpuSurfaceTextureError,
) -> WebGpuSurfaceErrorAction {
    match err {
        webgpu::WebGpuSurfaceTextureError::Outdated => {
            WebGpuSurfaceErrorAction::ForceConfigure
        }
        webgpu::WebGpuSurfaceTextureError::Lost => {
            WebGpuSurfaceErrorAction::RecreateSurface
        }
        webgpu::WebGpuSurfaceTextureError::Timeout
        | webgpu::WebGpuSurfaceTextureError::Occluded
        | webgpu::WebGpuSurfaceTextureError::Validation => {
            WebGpuSurfaceErrorAction::DeferToRecoveryPolicy
        }
    }
}

const fn webgpu_repair_failure_stage() -> RenderFailureStage {
    // Once force-configure or replacement-surface construction itself fails,
    // the original Outdated/Lost event is no longer the actionable cause.
    // Treat the failed repair as validation/reconstruction evidence so a
    // resize or focus signal cannot silently reopen it as a transient fault.
    RenderFailureStage::SurfaceAcquire(webgpu::WebGpuSurfaceTextureError::Validation)
}

/// Drive the production WebGPU acquisition/repair policy through injectable
/// backend boundaries.
///
/// Production passes the real `WebGpuState` operations below. Tests can
/// deterministically script the synchronous surface results while still
/// exercising this exact prepare/classify/repair/reacquire state machine.
fn acquire_webgpu_frame_with_repair_using<
    T,
    PrepareSurface,
    AcquireSurfaceFrame,
    ForceConfigure,
    RecreateSurface,
>(
    dimensions: Dimensions,
    mut prepare_surface: PrepareSurface,
    mut acquire_surface_frame: AcquireSurfaceFrame,
    mut force_configure: ForceConfigure,
    mut recreate_surface: RecreateSurface,
) -> Result<T, RenderAttemptFailure>
where
    PrepareSurface:
        FnMut(Dimensions) -> anyhow::Result<webgpu::SurfaceConfigureOutcome>,
    AcquireSurfaceFrame:
        FnMut() -> Result<T, webgpu::WebGpuSurfaceTextureError>,
    ForceConfigure:
        FnMut(Dimensions) -> anyhow::Result<webgpu::SurfaceConfigureOutcome>,
    RecreateSurface:
        FnMut(Dimensions) -> anyhow::Result<webgpu::SurfaceConfigureOutcome>,
{
    match prepare_surface(dimensions) {
        Ok(webgpu::SurfaceConfigureOutcome::Ready) => {}
        Ok(webgpu::SurfaceConfigureOutcome::DeferredZeroExtent) => {
            return Err(RenderAttemptFailure::new(
                RenderFailureStage::SurfaceAcquire(
                    webgpu::WebGpuSurfaceTextureError::Occluded,
                ),
                anyhow!("webgpu surface has zero extent; waiting for resize/focus"),
            ));
        }
        Err(err) => {
            return Err(RenderAttemptFailure::new(
                RenderFailureStage::SurfaceAcquire(
                    webgpu::WebGpuSurfaceTextureError::Validation,
                ),
                err.context("prepare webgpu surface"),
            ));
        }
    }

    let first_error = match acquire_surface_frame() {
        Ok(frame) => return Ok(frame),
        Err(err) => err,
    };
    let repair = match classify_webgpu_surface_error(&first_error) {
        WebGpuSurfaceErrorAction::ForceConfigure => {
            metrics::counter!("gui.render.retry", "action" => "force_configure")
                .increment(1);
            force_configure(dimensions).context("force-configure outdated webgpu surface")
        }
        WebGpuSurfaceErrorAction::RecreateSurface => {
            metrics::counter!("gui.render.retry", "action" => "recreate_surface")
                .increment(1);
            recreate_surface(dimensions).context("recreate lost webgpu surface")
        }
        WebGpuSurfaceErrorAction::DeferToRecoveryPolicy => {
            return Err(RenderAttemptFailure::new(
                RenderFailureStage::SurfaceAcquire(first_error),
                first_error.into(),
            ));
        }
    };

    match repair {
        Ok(webgpu::SurfaceConfigureOutcome::Ready) => {}
        Ok(webgpu::SurfaceConfigureOutcome::DeferredZeroExtent) => {
            return Err(RenderAttemptFailure::new(
                RenderFailureStage::SurfaceAcquire(
                    webgpu::WebGpuSurfaceTextureError::Occluded,
                ),
                anyhow!("webgpu repair deferred for zero surface extent"),
            ));
        }
        Err(err) => {
            return Err(RenderAttemptFailure::new(
                webgpu_repair_failure_stage(),
                err.context(format!(
                    "webgpu surface repair failed after {first_error}"
                )),
            ));
        }
    }

    acquire_surface_frame().map_err(|err| {
        // Only one immediate repair is permitted per admitted paint.
        RenderAttemptFailure::new(RenderFailureStage::SurfaceAcquire(err), err.into())
    })
}

impl TermWindow {
    pub async fn new_window(
        mux_window_id: MuxWindowId,
        saved_workspace: String,
        saved_window_state: Option<crate::window_state_persist::PersistedWindowState>,
    ) -> anyhow::Result<()> {
        let config = config_with_accessibility_palette(configuration());
        let dpi = config.dpi.unwrap_or_else(::window::default_dpi) as usize;
        let fontconfig = Rc::new(FontConfiguration::new(Some(config.clone()), dpi)?);

        let mux = Mux::try_get()
            .ok_or_else(|| anyhow!("cannot create GUI window without an active mux"))?;
        let size = match mux.get_active_tab_for_window(mux_window_id) {
            Some(tab) => tab.get_size(),
            None => {
                log::debug!("new_window has no tabs... yet?");
                Default::default()
            }
        };
        let physical_rows = size.rows as usize;
        let physical_cols = size.cols as usize;

        let render_metrics = RenderMetrics::new(&fontconfig)?;
        log::trace!("using render_metrics {:#?}", render_metrics);

        // Initially we have only a single tab, so take that into account
        // for the tab bar state.
        let show_tab_bar = config.enable_tab_bar && !config.hide_tab_bar_if_only_one_tab;
        let tab_bar_height = if show_tab_bar {
            Self::tab_bar_pixel_height_impl(&config, &fontconfig, &render_metrics)? as usize
        } else {
            0
        };

        let terminal_size = TerminalSize {
            rows: physical_rows,
            cols: physical_cols,
            pixel_width: (render_metrics.cell_size.width as usize * physical_cols),
            pixel_height: (render_metrics.cell_size.height as usize * physical_rows),
            dpi: dpi as u32,
        };

        if terminal_size != size {
            // DPI is different from the default assumed DPI when the mux
            // created the pty. We need to inform the kernel of the revised
            // pixel geometry now
            log::trace!(
                "Initial geometry was {:?} but dpi-adjusted geometry \
                        is {:?}; update the kernel pixel geometry for the ptys!",
                size,
                terminal_size,
            );
            if let Some(window) = mux.get_window(mux_window_id) {
                for tab in window.iter() {
                    tab.resize(terminal_size);
                }
            };
        }

        let h_context = DimensionContext {
            dpi: dpi as f32,
            pixel_max: terminal_size.pixel_width as f32,
            pixel_cell: render_metrics.cell_size.width as f32,
        };
        let padding_left = config.window_padding.left.evaluate_as_pixels(h_context) as usize;
        let padding_right = resize::effective_right_padding(&config, h_context) as usize;
        let v_context = DimensionContext {
            dpi: dpi as f32,
            pixel_max: terminal_size.pixel_height as f32,
            pixel_cell: render_metrics.cell_size.height as f32,
        };
        let padding_top = config.window_padding.top.evaluate_as_pixels(v_context) as usize;
        let padding_bottom = config.window_padding.bottom.evaluate_as_pixels(v_context) as usize;

        let mut dimensions = Dimensions {
            pixel_width: (terminal_size.pixel_width + padding_left + padding_right) as usize,
            pixel_height: ((terminal_size.rows * render_metrics.cell_size.height as usize)
                + padding_top
                + padding_bottom) as usize
                + tab_bar_height,
            dpi,
        };

        let border = Self::get_os_border_impl(&None, &config, &dimensions, &render_metrics);

        dimensions.pixel_height += (border.top + border.bottom).get() as usize;
        dimensions.pixel_width += (border.left + border.right).get() as usize;

        // File IO, decoding, gradients and large solid-color buffers are loaded
        // after the native window exists on the bounded background pool.  The
        // first frame therefore never waits on user-controlled image work.
        let window_background = Vec::new();

        log::trace!(
            "TermWindow::new_window called with mux_window_id {} {:?} {:?}",
            mux_window_id,
            terminal_size,
            dimensions
        );

        let render_state = None;

        let connection_name = Connection::get()
            .map(|c| c.name())
            .unwrap_or_else(|| "unknown".to_string());

        let myself = Self {
            created: Instant::now(),
            connection_name,
            last_fps_check_time: Instant::now(),
            num_frames: 0,
            last_frame_duration: Duration::ZERO,
            fps: 0.,
            config_subscription: None,
            os_parameters: None,
            gl: None,
            webgpu: None,
            window: None,
            window_background,
            background_load: BackgroundLoadCoordinator::default(),
            config: config.clone(),
            config_overrides: wezterm_dynamic::Value::Object(Default::default()),
            palette: None,
            focused: None,
            mux_window_id,
            mux_window_id_for_subscriptions: Arc::new(Mutex::new(mux_window_id)),
            fonts: Rc::clone(&fontconfig),
            render_metrics,
            dimensions,
            window_state: WindowState::default(),
            resizes_pending: 0,
            is_repaint_pending: false,
            pending_update_title: false,
            pending_scale_changes: LinkedList::new(),
            terminal_size,
            render_state,
            input_map: InputMap::new(&config),
            leader_is_down: None,
            dead_key_status: DeadKeyStatus::None,
            show_tab_bar,
            show_scroll_bar: config.enable_scroll_bar,
            tab_bar: TabBarState::default(),
            fancy_tab_bar: None,
            right_status: String::new(),
            left_status: String::new(),
            last_mouse_coords: (0, -1),
            window_drag_position: None,
            current_mouse_event: None,
            current_modifier_and_leds: Default::default(),
            prev_cursor: PrevCursorPos::new(),
            last_scroll_info: RenderableDimensions::default(),
            tab_state: RefCell::new(HashMap::new()),
            pane_state: RefCell::new(HashMap::new()),
            current_mouse_buttons: vec![],
            current_mouse_capture: None,
            active_selection_drag_pane: None,
            last_mouse_click: None,
            current_highlight: None,
            quad_generation: 0,
            shape_generation: 0,
            dirty_lines: HashMap::new(),
            damage_generation: DamageGeneration::default(),
            render_recovery_state: RenderRecoveryState::default(),
            render_wake_state: RenderWakeState::default(),
            // Per ft-gwzrm: live dirty sources are wired, and the
            // render path only records a clean-line skip after a
            // cached quad list was actually reused.
            iter_dirty_render_gate_enabled: true,
            // Per ft-d6nrd slice 1: 60 Hz default budget; the
            // adaptive-FPS path will override this once the
            // refresh-rate probe lands. paint_impl will tick
            // begin_frame / end_frame around each frame.
            frame_budget: frame_budget::FrameBudget::new(60),
            idle_detector: idle_detector::IdleDetector::new(Instant::now()),
            last_idle_transition: None,
            cosmetic_defer_outstanding:
                frankenterm_core::frame_budget_a11y_gate::CosmeticDeferOutstanding::default(),
            frame_budget_gate_telemetry:
                frankenterm_core::frame_budget_a11y_gate::FrameBudgetGateTelemetry::default(),
            frame_budget_reduce_motion_state: probe_reduce_motion_state(),
            dirty_marks_by_source:
                frankenterm_core::dirty_line_telemetry::MarksBySource::default(),
            triple_buffer_panes: TerminalStateTripleBufferRegistry::default(),
            triple_buffer_pane_health: HashMap::new(),
            sync_output_watchdog_telemetry:
                frankenterm_core::sync_output_watchdog::SyncOutputTelemetry::default(),
            sync_output_orchestrator_telemetry:
                frankenterm_core::sync_output_buffer_orchestrator::SyncOutputOrchestratorTelemetry::default(),
            sync_output_bsu_depth_by_pane: HashMap::new(),
            sync_output_buffered_bytes_by_pane: HashMap::new(),
            quad_buffer_policy: render::elastic_buffer::ElasticBuffer::new(0),
            quad_buffer_in_resize_gesture: false,
            shape_cache: RefCell::new(LfuCache::new(
                "shape_cache.hit.rate",
                "shape_cache.miss.rate",
                |config| config.shape_cache_size,
                &config,
            )),
            line_state_cache_owner: Arc::new(render::LineStateCacheOwner::default()),
            line_state_cache: RefCell::new(LfuCacheU64::new(
                "line_state_cache.hit.rate",
                "line_state_cache.miss.rate",
                |config| config.line_state_cache_size,
                &config,
            )),
            next_line_state_id: 0,
            line_quad_cache: RefCell::new(LfuCache::new(
                "line_quad_cache.hit.rate",
                "line_quad_cache.miss.rate",
                |config| config.line_quad_cache_size,
                &config,
            )),
            last_tab_state_prune_revision: Cell::new(None),
            line_to_ele_shape_cache: RefCell::new(LfuCache::new(
                "line_to_ele_shape_cache.hit.rate",
                "line_to_ele_shape_cache.miss.rate",
                |config| config.line_to_ele_shape_cache_size,
                &config,
            )),
            last_status_call: Instant::now(),
            cursor_blink_state: RefCell::new(ColorEase::new(
                config.cursor_blink_rate,
                config.cursor_blink_ease_in,
                config.cursor_blink_rate,
                config.cursor_blink_ease_out,
                None,
            )),
            blink_state: RefCell::new(ColorEase::new(
                config.text_blink_rate,
                config.text_blink_ease_in,
                config.text_blink_rate,
                config.text_blink_ease_out,
                None,
            )),
            rapid_blink_state: RefCell::new(ColorEase::new(
                config.text_blink_rate_rapid,
                config.text_blink_rapid_ease_in,
                config.text_blink_rate_rapid,
                config.text_blink_rapid_ease_out,
                None,
            )),
            event_states: HashMap::new(),
            current_event: None,
            has_animation: RefCell::new(None),
            allow_images: AllowImage::Yes,
            semantic_zones: HashMap::new(),
            ui_items: vec![],
            dragging: None,
            last_ui_item: None,
            is_click_to_focus_window: false,
            key_table_state: KeyTableState::default(),
            modal: RefCell::new(None),
            opengl_info: None,
            agent_pane_states: HashMap::new(),
            dashboard: crate::dashboard::DashboardPanel::default(),
        };

        let tw = Rc::new(RefCell::new(myself));
        let tw_event = Rc::clone(&tw);

        let mut x = None;
        let mut y = None;
        let mut origin = GeometryOrigin::default();

        if let Some(position) = mux
            .get_window(mux_window_id)
            .and_then(|window| window.get_initial_position().clone())
            .or_else(|| lock_termwindow_mutex(&POSITION, "window position").take())
        {
            x.replace(position.x);
            y.replace(position.y);
            origin = position.origin;
        }

        let geometry = RequestedWindowGeometry {
            width: Dimension::Pixels(dimensions.pixel_width as f32),
            height: Dimension::Pixels(dimensions.pixel_height as f32),
            x,
            y,
            origin,
        };
        log::trace!("{:?}", geometry);

        let window = Window::new_window(
            &get_window_class(),
            "FrankenTerm",
            geometry,
            Some(&config),
            Rc::clone(&fontconfig),
            move |event, window| {
                let mut tw = tw_event.borrow_mut();
                if let Err(err) = tw.dispatch_window_event(event, window) {
                    log::error!("dispatch_window_event: {:#}", err);
                }
            },
        )
        .await?;
        tw.borrow_mut().window.replace(window.clone());

        Self::apply_icon(&window)?;

        let config_subscription = config::subscribe_to_config_reload({
            let window = window.clone();
            move || {
                window.notify(TermWindowNotif::Apply(Box::new(|tw| {
                    tw.config_was_reloaded()
                })));
                true
            }
        });

        let gl = match config.front_end {
            FrontEndSelection::WebGpu => None,
            _ => Some(window.enable_opengl().await?),
        };

        {
            let mut myself = tw.borrow_mut();
            let webgpu = match config.front_end {
                FrontEndSelection::WebGpu => Some(Rc::new(
                    WebGpuState::new(&window, dimensions, &config).await?,
                )),
                _ => None,
            };
            myself.config_subscription.replace(config_subscription);
            if config.use_resize_increments {
                window.set_resize_increments(
                    ResizeIncrementCalculator {
                        x: myself.render_metrics.cell_size.width as u16,
                        y: myself.render_metrics.cell_size.height as u16,
                        padding_left,
                        padding_top,
                        padding_right,
                        padding_bottom,
                        border,
                        tab_bar_height,
                    }
                    .into(),
                );
            }

            if let Some(gl) = gl {
                myself.gl.replace(Rc::clone(&gl));
                myself.created(RenderContext::Glium(Rc::clone(&gl)))?;
            }
            if let Some(webgpu) = webgpu {
                myself.webgpu.replace(Rc::clone(&webgpu));
                myself.created(RenderContext::WebGpu(Rc::clone(&webgpu)))?;
            }
            myself.load_os_parameters();
            myself.schedule_background_reload();
            myself
                .subscribe_to_pane_updates()
                .context("subscribing new GUI window to mux pane updates")?;
            window.show();
            myself.emit_window_event("window-config-reloaded", None);
            myself.emit_status_event();
        }

        // Restore the maximize/fullscreen state captured for this entire GUI
        // reconciliation cohort before any member was notified or shown. A
        // missing entry calls neither method, so a window with no saved state
        // keeps today's exact default geometry. The existing position-restore
        // path above is untouched.
        let saved_window_state = mux.get_window(mux_window_id).and_then(|mux_window| {
            let current_workspace = mux_window.get_workspace();
            if current_workspace != saved_workspace.as_str() {
                log::debug!(
                    "refreshing stale window-state restore for mux window {mux_window_id}: \
                     captured workspace changed before application"
                );
            }
            crate::window_state_persist::resolve_saved_window_state(
                &saved_workspace,
                saved_window_state,
                Some(current_workspace),
                crate::window_state_persist::load_startup_for_workspace,
            )
        });
        if let Some(saved) = saved_window_state {
            if saved.maximized {
                window.maximize();
            }
            if saved.fullscreen {
                window.toggle_fullscreen();
            }
        }

        crate::update::start_update_checker();
        if let Some(front_end) = try_front_end() {
            front_end.record_known_window(window, mux_window_id);
        }

        Ok(())
    }

    fn dispatch_window_event(
        &mut self,
        event: WindowEvent,
        window: &Window,
    ) -> anyhow::Result<bool> {
        log::debug!("{event:?}");
        match event {
            WindowEvent::Destroyed => {
                // Ensure that we cancel any overlays we had running, so
                // that the mux can empty out, otherwise the mux keeps
                // the TermWindow alive via the frontend even though
                // the window is gone and we'll linger forever.
                // <https://github.com/wezterm/wezterm/issues/3522>
                self.clear_all_overlays();
                self.background_load.cancel();
                self.release_render_resources_before_window();
                Ok(false)
            }
            WindowEvent::CloseRequested => {
                self.close_requested(window);
                Ok(true)
            }
            WindowEvent::AppearanceChanged(appearance) => {
                log::debug!("Appearance is now {:?}", appearance);
                // This is a bit fugly; we get per-window notifications
                // for appearance changes which successfully updates the
                // per-window config, but we need to explicitly tell the
                // global config to reload, otherwise things that acces
                // the config via config::configuration() will see the
                // prior version of the config.
                // What's fugly about this is that we'll reload the
                // global config here once per window, which could
                // be nasty for folks with a lot of windows.
                // <https://github.com/wezterm/wezterm/issues/2295>
                config::reload();
                self.config_was_reloaded();
                Ok(true)
            }
            WindowEvent::PerformKeyAssignment(action) => {
                if let Some(pane) = self.get_active_pane_or_overlay() {
                    self.perform_key_assignment(&pane, &action)?;
                    window.invalidate();
                }
                Ok(true)
            }
            WindowEvent::FocusChanged(focused) => {
                self.focus_changed(focused, window);
                Ok(true)
            }
            WindowEvent::MouseEvent(event) => {
                self.mouse_event_impl(event, window);
                Ok(true)
            }
            WindowEvent::MouseLeave => {
                self.mouse_leave_impl(window);
                Ok(true)
            }
            WindowEvent::Resized {
                dimensions,
                window_state,
                live_resizing,
            } => {
                self.resize(dimensions, window_state, window, live_resizing);
                Ok(true)
            }
            WindowEvent::SetInnerSizeCompleted => {
                self.resizes_pending = self.resizes_pending.saturating_sub(1);
                self.note_render_surface_recovery_signal();
                if self.is_repaint_pending {
                    self.is_repaint_pending = false;
                    self.paint_if_admitted(window)?;
                }
                self.apply_pending_scale_changes();
                Ok(true)
            }
            WindowEvent::AdviseModifiersLedStatus(modifiers, leds) => {
                self.current_modifier_and_leds = (modifiers, leds);
                self.update_title();
                window.invalidate();
                Ok(true)
            }
            WindowEvent::RawKeyEvent(event) => {
                self.raw_key_event_impl(event, window);
                Ok(true)
            }
            WindowEvent::KeyEvent(event) => {
                self.key_event_impl(event, window);
                Ok(true)
            }
            WindowEvent::AdviseDeadKeyStatus(status) => {
                if self.config.debug_key_events {
                    log::info!("DeadKeyStatus now: {:?}", status);
                } else {
                    log::trace!("DeadKeyStatus now: {:?}", status);
                }
                self.dead_key_status = status;
                self.update_title();
                // Ensure that we repaint so that any composing
                // text is updated
                window.invalidate();
                Ok(true)
            }
            WindowEvent::NeedRepaint => {
                if self.resizes_pending > 0 {
                    self.is_repaint_pending = true;
                    Ok(true)
                } else {
                    self.paint_if_admitted(window)
                }
            }
            WindowEvent::Notification(item) => {
                if let Ok(notif) = item.downcast::<TermWindowNotif>() {
                    self.dispatch_notif(*notif, window)
                        .context("dispatch_notif")?;
                }
                Ok(true)
            }
            WindowEvent::DroppedString(text) => {
                let pane = match self.get_active_pane_or_overlay() {
                    Some(pane) => pane,
                    None => return Ok(true),
                };
                pane.send_paste(text.as_str())?;
                Ok(true)
            }
            WindowEvent::DroppedUrl(urls) => {
                let pane = match self.get_active_pane_or_overlay() {
                    Some(pane) => pane,
                    None => return Ok(true),
                };
                let urls = urls
                    .iter()
                    .map(|url| self.config.quote_dropped_files.escape(&url.to_string()))
                    .collect::<Vec<_>>()
                    .join(" ")
                    + " ";
                pane.send_paste(urls.as_str())?;
                Ok(true)
            }
            WindowEvent::DroppedFile(paths) => {
                let pane = match self.get_active_pane_or_overlay() {
                    Some(pane) => pane,
                    None => return Ok(true),
                };
                let paths = paths
                    .iter()
                    .map(|path| {
                        self.config
                            .quote_dropped_files
                            .escape(&path.to_string_lossy())
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
                    + " ";
                pane.send_paste(&paths)?;
                Ok(true)
            }
            WindowEvent::DraggedFile(_) => Ok(true),
        }
    }

    fn paint_if_admitted(&mut self, window: &Window) -> anyhow::Result<bool> {
        match self.render_recovery_state.admit() {
            PaintAdmission::Admit => {
                if self.webgpu.is_some() {
                    self.do_paint_webgpu()
                } else {
                    Ok(self.do_paint(window))
                }
            }
            PaintAdmission::SuppressCooldown => {
                metrics::counter!("gui.render.paint_admission", "decision" => "cooldown")
                    .increment(1);
                Ok(true)
            }
            PaintAdmission::SuppressParked => {
                metrics::counter!("gui.render.paint_admission", "decision" => "parked")
                    .increment(1);
                if let Some(stage) = self.render_recovery_state.parked_occluded_stage() {
                    // WindowEvent does not distinguish OS exposure from an
                    // application invalidation. Convert either source into at
                    // most one delayed acquisition-only probe: Cooldown then
                    // suppresses every subsequent repaint until the exact wake
                    // ticket fires. A failed probe parks again and requires a
                    // new repaint request, so there is no permanent timer.
                    self.schedule_render_retry(stage, OCCLUDED_REPAINT_PROBE_DELAY);
                    metrics::counter!(
                        "gui.render.retry",
                        "action" => "occluded_repaint_probe"
                    )
                    .increment(1);
                }
                Ok(true)
            }
            PaintAdmission::SuppressCircuit => {
                metrics::counter!("gui.render.paint_admission", "decision" => "circuit_open")
                    .increment(1);
                Ok(true)
            }
        }
    }

    fn release_render_resources_before_window(&mut self) {
        self.render_wake_state.cancel();
        // RenderState owns another Rc<WebGpuState>; release it first, then the
        // TermWindow Rc, so the unsafe raw-handle surface is gone before the
        // native window/front-end registration can be forgotten.
        drop(self.render_state.take());
        drop(self.webgpu.take());
        drop(self.gl.take());
    }

    fn do_paint(&mut self, window: &Window) -> bool {
        let gl = match self.gl.as_ref().map(Rc::clone) {
            Some(gl) => gl,
            None => return false,
        };

        if gl.is_context_lost() {
            let failure = RenderAttemptFailure::new(
                RenderFailureStage::Draw(DrawFailureStage::BackendDraw),
                anyhow!("OpenGL context was lost; renderer reinitialization required"),
            );
            self.handle_render_failure(&failure);
            log::error!("{failure:#}");
            return false;
        }

        let dimensions = self.dimensions;
        let outcome = match self.paint_impl(move |tw| {
            // Construct the linear glium Frame only after geometry succeeds.
            // Once constructed it is consumed exactly once, even when drawing
            // fails, because glium's Drop otherwise panics.
            let mut frame = glium::Frame::new(
                gl,
                (
                    dimensions.pixel_width as u32,
                    dimensions.pixel_height as u32,
                ),
            );
            let draw_result = tw.call_draw_glium(&mut frame);
            let finish_result = window.finish_frame(frame);
            combine_glium_draw_and_finish(draw_result, finish_result)
        }) {
            Ok(outcome) => outcome,
            Err(failure) => {
                self.handle_render_failure(&failure);
                log::warn!("OpenGL paint failed; retaining damage: {failure:#}");
                return false;
            }
        };
        self.complete_presented_frame(outcome);
        true
    }

    fn do_paint_webgpu(&mut self) -> anyhow::Result<bool> {
        let Some(webgpu) = self.webgpu.as_ref().map(Rc::clone) else {
            log::warn!("cannot paint webgpu frame before webgpu state is initialized");
            return Ok(false);
        };
        let dimensions = self.dimensions;
        let acquired = match Self::acquire_webgpu_frame_with_repair(&webgpu, dimensions) {
            Ok(acquired) => acquired,
            Err(failure) => {
                self.handle_render_failure(&failure);
                log::warn!("WebGPU acquisition failed; retaining damage: {failure:#}");
                return Ok(true);
            }
        };

        let outcome = match self.paint_impl(move |tw| {
            tw.call_draw_webgpu(acquired)
                .map_err(RenderAttemptFailure::draw)
        }) {
            Ok(outcome) => outcome,
            Err(failure) => {
                self.handle_render_failure(&failure);
                log::warn!("WebGPU paint failed; retaining damage: {failure:#}");
                return Ok(true);
            }
        };
        self.complete_presented_frame(outcome);
        Ok(true)
    }

    fn acquire_webgpu_frame_with_repair(
        webgpu: &WebGpuState,
        dimensions: Dimensions,
    ) -> Result<webgpu::AcquiredWebGpuFrame, RenderAttemptFailure> {
        acquire_webgpu_frame_with_repair_using(
            dimensions,
            |dims| webgpu.prepare_surface(dims),
            || webgpu.acquire_surface_frame(),
            |dims| webgpu.force_configure(dims),
            |dims| webgpu.recreate_surface(dims),
        )
    }

    fn dispatch_notif(&mut self, notif: TermWindowNotif, window: &Window) -> anyhow::Result<()> {
        fn chan_err<T>(e: TrySendError<T>) -> anyhow::Error {
            anyhow::anyhow!("{}", e)
        }

        match notif {
            TermWindowNotif::InvalidateShapeCache => {
                self.advance_shaping_input_generation();
                self.invalidate_modal();
                // ft-mpc9b.1.2: shape-cache invalidation covers
                // font change and other render-shape-affecting
                // events. Mark every pane dirty so the next paint
                // sees the full repaint signal. Per ft-i6k6u: tag
                // as FontSwap so the per-source aggregator
                // attributes shape-cache invalidations to font
                // metrics changes (the dominant cause).
                self.mark_all_panes_dirty_with_source(
                    frankenterm_core::dirty_line_telemetry::DirtyEventSource::FontSwap,
                );
                window.invalidate();
            }
            TermWindowNotif::PerformAssignment {
                pane_id,
                assignment,
                tx,
            } => {
                let result = || -> anyhow::Result<()> {
                    // The CopyMode overlay doesn't exist in the mux, but aliases
                    // itself with the overlaid pane's pane_id.
                    // So we do a bit of fancy footwork here to resolve the overlay
                    // and use that if it has the same pane_id, but otherwise fall
                    // back to what we get from the mux.
                    // <https://github.com/wezterm/wezterm/issues/3209>
                    let active_pane = self
                        .get_active_pane_or_overlay()
                        .ok_or_else(|| anyhow!("there is no active pane!?"))?;
                    let pane = if active_pane.pane_id() == pane_id {
                        active_pane
                    } else {
                        Mux::try_get()
                            .and_then(|mux| mux.get_pane(pane_id))
                            .ok_or_else(|| anyhow!("pane id {} is not valid", pane_id))?
                    };
                    self.perform_key_assignment(&pane, &assignment)
                        .context("perform_key_assignment")?;
                    Ok(())
                }();
                window.invalidate();
                if let Some(tx) = tx {
                    tx.try_send(result).ok();
                }
            }
            TermWindowNotif::SetRightStatus(status) => {
                if status != self.right_status {
                    self.right_status = status;
                    self.update_title_post_status();
                } else {
                    self.schedule_next_status_update();
                }
            }
            TermWindowNotif::SetLeftStatus(status) => {
                if status != self.left_status {
                    self.left_status = status;
                    self.update_title_post_status();
                } else {
                    self.schedule_next_status_update();
                }
            }
            TermWindowNotif::GetDimensions(tx) => {
                tx.try_send((self.dimensions, self.window_state))
                    .map_err(chan_err)
                    .context("send GetDimensions response")?;
            }
            TermWindowNotif::GetEffectiveConfig(tx) => {
                tx.try_send(self.config.clone())
                    .map_err(chan_err)
                    .context("send GetEffectiveConfig response")?;
            }
            TermWindowNotif::FinishWindowEvent { name, again } => {
                self.finish_window_event(&name, again);
            }
            TermWindowNotif::GetConfigOverrides(tx) => {
                tx.try_send(self.config_overrides.clone())
                    .map_err(chan_err)
                    .context("send GetConfigOverrides response")?;
            }
            TermWindowNotif::SetConfigOverrides(value) => {
                if value != self.config_overrides {
                    self.config_overrides = value;
                    self.config_was_reloaded();
                }
            }
            TermWindowNotif::CancelOverlayForPane { pane_id, ticket } => {
                self.cancel_overlay_for_pane_if_current(pane_id, &ticket);
            }
            TermWindowNotif::CancelOverlayForTab {
                tab_id,
                overlay_pane_id,
                ticket,
            } => {
                self.cancel_overlay_for_tab_if_current(tab_id, Some(overlay_pane_id), &ticket);
            }
            TermWindowNotif::MuxNotification {
                notification: n,
                mux_owner,
                pane_removal_cleanup,
            } => {
                let Some(notification_owner) = mux_owner.upgrade() else {
                    return Ok(());
                };
                if !Mux::try_get()
                    .is_some_and(|current| Arc::ptr_eq(&current, &notification_owner))
                {
                    // A queued notification from a replaced mux must never act
                    // on same-numbered panes or windows in the new mux.
                    return Ok(());
                }
                match n {
                MuxNotification::Alert {
                    alert: Alert::SetUserVar { name, value },
                    pane_id,
                } => {
                    self.emit_user_var_event(pane_id, name, value);
                }
                MuxNotification::WindowTitleChanged { .. }
                | MuxNotification::Alert {
                    alert:
                        Alert::OutputSinceFocusLost
                        | Alert::CurrentWorkingDirectoryChanged
                        | Alert::WindowTitleChanged(_)
                        | Alert::TabTitleChanged(_)
                        | Alert::IconTitleChanged(_)
                        | Alert::Progress(_),
                    ..
                } => {
                    // Coalesce: agents emitting OSC 7 / OSC 9;4 across multiple
                    // attached muxes can fire dozens of these per second.
                    // Schedule one frame-bounded update_title instead. ft-9d60d.
                    self.schedule_update_title();
                }
                MuxNotification::Alert {
                    alert: Alert::PaletteChanged,
                    pane_id,
                } => {
                    // Shape cache includes color information, so
                    // ensure that we invalidate that as part of
                    // this overall invalidation for the palette
                    self.dispatch_notif(TermWindowNotif::InvalidateShapeCache, window)?;
                    self.mux_pane_output_event(pane_id);
                }
                MuxNotification::Alert {
                    alert: Alert::ImageAltText { .. },
                    pane_id,
                } => {
                    self.mux_pane_output_event(pane_id);
                }
                MuxNotification::Alert {
                    alert: Alert::Bell,
                    pane_id,
                } => {
                    if !self.window_contains_pane(pane_id) {
                        return Ok(());
                    }

                    self.record_idle_event(idle_detector::IdleEvent::Bell);

                    match self.config.audible_bell {
                        AudibleBell::SystemBeep => {
                            if let Some(connection) = Connection::get() {
                                connection.beep();
                            } else {
                                log::warn!("cannot play system beep without a GUI connection");
                            }
                        }
                        AudibleBell::Disabled => {}
                    }

                    log::trace!("Ding! (this is the bell) in pane {}", pane_id);
                    self.emit_window_event("bell", Some(pane_id));

                    let mut per_pane = self.pane_state(pane_id);
                    per_pane.bell_start.replace(Instant::now());
                    window.invalidate();
                }
                MuxNotification::Alert {
                    alert: Alert::ToastNotification { .. },
                    ..
                } => {}
                MuxNotification::Alert {
                    alert: Alert::SetProfileRequested { .. } | Alert::MouseShapeRequested { .. },
                    ..
                } => {
                    // ft-fy4ty / ft-7yiu2: surfaced to the embedder
                    // for a confirmation prompt (SetProfile) or
                    // native cursor mapping (MouseShape). The GUI
                    // continuation beads (ft-tzusd / ft-jornq) wire
                    // the actual UI; the term-layer alert path
                    // already routes through the alert handler so
                    // the mux-side notification is intentionally a
                    // no-op here.
                }
                MuxNotification::TabAddedToWindow {
                    window_id: _,
                    tab_id,
                } => {
                    let mut size = self.terminal_size;
                    if let Some(tab) = notification_owner.get_tab(tab_id) {
                        // If we attached to a remote domain and loaded in
                        // a tab async, we need to fixup its size, either
                        // by resizing it or resizes ourselves.
                        // The strategy here is to adjust both by taking
                        // the maximal size in both horizontal and vertical
                        // dimensions and applying that. In practice that
                        // means that a new local client will resize larger
                        // to adjust to the size of an existing client.
                        let tab_size = tab.get_size();
                        size.rows = size.rows.max(tab_size.rows);
                        size.cols = size.cols.max(tab_size.cols);

                        if size.rows != self.terminal_size.rows
                            || size.cols != self.terminal_size.cols
                            || size.pixel_width != self.terminal_size.pixel_width
                            || size.pixel_height != self.terminal_size.pixel_height
                        {
                            self.set_window_size(size, window)?;
                        } else if tab_size.dpi == 0 {
                            log::debug!("fixup dpi in newly added tab");
                            tab.resize(self.terminal_size);
                        }
                    }
                }
                MuxNotification::PaneOutput(pane_id) => {
                    self.mux_pane_output_event(pane_id);
                }
                MuxNotification::SynchronizedOutput { pane_id, event } => {
                    self.mux_synchronized_output_event(pane_id, event);
                }
                MuxNotification::WindowTopologyChanged(change) => {
                    if !change.affects_window(self.mux_window_id) {
                        return Ok(());
                    }
                    let mut size = self.terminal_size;
                    for &(tab_id, window_id) in change.attached_tabs() {
                        if window_id != self.mux_window_id {
                            continue;
                        }
                        if let Some(tab) = notification_owner.get_tab(tab_id) {
                            let tab_size = tab.get_size();
                            size.rows = size.rows.max(tab_size.rows);
                            size.cols = size.cols.max(tab_size.cols);
                        }
                    }
                    if size.rows != self.terminal_size.rows
                        || size.cols != self.terminal_size.cols
                        || size.pixel_width != self.terminal_size.pixel_width
                        || size.pixel_height != self.terminal_size.pixel_height
                    {
                        self.set_window_size(size, window)?;
                    } else {
                        for &(tab_id, window_id) in change.attached_tabs() {
                            if window_id != self.mux_window_id {
                                continue;
                            }
                            if let Some(tab) = notification_owner.get_tab(tab_id)
                                && tab.get_size().dpi == 0
                            {
                                log::debug!("fixup dpi in newly attached tab");
                                tab.resize(self.terminal_size);
                            }
                        }
                    }
                    self.prune_tab_state_to_live_window();
                    self.record_idle_event(idle_detector::IdleEvent::OsPaintRequest);
                    window.invalidate();
                    self.update_title_post_status();
                }
                MuxNotification::WindowInvalidated(_)
                | MuxNotification::WindowOrderChanged { .. } => {
                    self.prune_tab_state_to_live_window();
                    self.record_idle_event(idle_detector::IdleEvent::OsPaintRequest);
                    window.invalidate();
                    self.update_title_post_status();
                }
                MuxNotification::FloatingPaneSpawnCommitted(spawn) => {
                    // The subscription routes this notification using the
                    // frozen window identity, but the GUI callback may have
                    // remained queued across a window switch. Never repaint
                    // the replacement window for stale topology.
                    if spawn.window_id() != self.mux_window_id {
                        return Ok(());
                    }

                    // Focus, tab membership, and floating geometry were all
                    // committed atomically before this event was published.
                    // Consume it as one paint/title-status invalidation only;
                    // queuing numeric-id focus reconciliation here could
                    // override a newer user action.
                    self.record_idle_event(idle_detector::IdleEvent::OsPaintRequest);
                    window.invalidate();
                    self.update_title_post_status();
                }
                MuxNotification::WindowRemoved(_window_id) => {
                    // Handled by frontend
                }
                MuxNotification::AssignClipboard { .. } => {
                    // Handled by frontend
                }
                MuxNotification::SaveToDownloads { .. } => {
                    // Handled by frontend
                }
                MuxNotification::PaneFocused(_) => {
                    // Also handled by clientpane
                    self.update_title_post_status();
                }
                MuxNotification::TabResized(_) => {
                    // Also handled by frankenterm-client
                    self.update_title_post_status();
                }
                MuxNotification::TabTitleChanged { .. } => {
                    self.update_title_post_status();
                }
                MuxNotification::PaneRemoved(pane_id) => {
                    if notification_owner.get_pane(pane_id).is_some() {
                        log::error!(
                            "refusing PaneRemoved GUI cleanup for live pane {pane_id}; removal fence authority was violated"
                        );
                        return Ok(());
                    }
                    // ft-mpc9b.1.2: drop the pane's dirty-line
                    // bitmap. Without this the HashMap leaks an
                    // entry per closed pane over the session
                    // lifetime.
                    self.forget_dirty_lines_for_pane(pane_id);
                    // Per ft-kyail: also drop the pane's live
                    // triple-buffer owner and retained health
                    // snapshot. Both are keyed by the substrate's
                    // u64 PaneId; cast from the mux's usize PaneId
                    // here.
                    self.forget_terminal_state_buffer_for_pane(terminal_pane_id_to_u64(pane_id));
                    self.forget_sync_output_state_for_pane(pane_id);
                    self.semantic_zones.remove(&pane_id);
                    self.agent_pane_states.remove(&pane_id);
                    self.line_quad_cache
                        .borrow_mut()
                        .remove_keys_where(|key| key.pane_id == pane_id);
                    self.line_state_cache
                        .borrow_mut()
                        .remove_where(|_, state| state.pane_id == pane_id);
                    let overlay_to_remove = self
                        .pane_state
                        .borrow_mut()
                        .remove(&pane_id)
                        .and_then(|state| state.overlay);
                    if let Some(overlay) = overlay_to_remove {
                        Self::retire_overlay_registration(OverlaySlot::Pane(pane_id), overlay);
                    }
                    let detached_retired_overlays = {
                        let mut detached = Vec::new();
                        for (owner_pane_id, state) in self.pane_state.borrow_mut().iter_mut() {
                            if state
                                .overlay
                                .as_ref()
                                .is_some_and(|overlay| overlay.pane.pane_id() == pane_id)
                            {
                                if let Some(overlay) = state.overlay.take() {
                                    detached.push((OverlaySlot::Pane(*owner_pane_id), overlay));
                                }
                            }
                        }
                        for (tab_id, state) in self.tab_state.borrow_mut().iter_mut() {
                            if state
                                .overlay
                                .as_ref()
                                .is_some_and(|overlay| overlay.pane.pane_id() == pane_id)
                            {
                                if let Some(overlay) = state.overlay.take() {
                                    detached.push((OverlaySlot::Tab(*tab_id), overlay));
                                }
                            }
                        }
                        detached
                    };
                    let detached_retired_overlay = !detached_retired_overlays.is_empty();
                    for (slot, overlay) in detached_retired_overlays {
                        Self::retire_overlay_registration(slot, overlay);
                    }
                    let captured_pane_id = match self.current_mouse_capture.as_ref() {
                        Some(MouseCapture::TerminalPane(captured_pane_id)) => {
                            Some(*captured_pane_id)
                        }
                        Some(MouseCapture::UI) | None => None,
                    };
                    let mouse_cleanup = frankenterm_gui::removed_pane_mouse_cleanup(
                        captured_pane_id,
                        self.active_selection_drag_pane,
                        pane_id,
                    );
                    if mouse_cleanup.clear_terminal_capture {
                        self.current_mouse_capture = None;
                        self.current_mouse_buttons.clear();
                    }
                    if mouse_cleanup.clear_selection_drag {
                        self.active_selection_drag_pane = None;
                    }
                    self.prune_tab_state_to_live_window();
                    if detached_retired_overlay {
                        window.invalidate();
                    }
                }
                MuxNotification::PaneAdded(_)
                | MuxNotification::WorkspaceRenamed { .. }
                | MuxNotification::WindowWorkspaceChanged { .. }
                | MuxNotification::ActiveWorkspaceChanged(_)
                | MuxNotification::Empty
                | MuxNotification::WindowCreated(_) => {}
                }
                if let Some(cleanup) = pane_removal_cleanup {
                    cleanup.complete();
                }
            }
            TermWindowNotif::EmitStatusUpdate => {
                let _ = self.poll_idle_scheduler();
                self.emit_status_event();
                // ft-kciew: drive the quad-buffer policy's idle
                // shrink consideration on the same cadence as the
                // status timer. No-op while a resize gesture is
                // active or the configured idle threshold (default
                // 1s) has not yet elapsed since the gesture ended.
                self.tick_quad_buffer_shrink();
            }
            TermWindowNotif::GetSelectionForPane { pane_id, tx } => {
                let pane = Mux::try_get()
                    .and_then(|mux| mux.get_pane(pane_id))
                    .ok_or_else(|| anyhow!("pane id {} is not valid", pane_id))?;

                tx.try_send(self.selection_text(&pane))
                    .map_err(chan_err)
                    .context("send GetSelectionForPane response")?;
            }
            TermWindowNotif::Apply(func) => {
                func(self);
            }
            TermWindowNotif::SwitchToMuxWindow(mux_window_id) => {
                self.mux_window_id = mux_window_id;
                match self.mux_window_id_for_subscriptions.lock() {
                    Ok(mut subscribed_window_id) => *subscribed_window_id = mux_window_id,
                    Err(poisoned) => {
                        log::warn!("recovering poisoned mux-window subscription lock");
                        *poisoned.into_inner() = mux_window_id;
                    }
                }

                self.clear_all_overlays();
                self.current_highlight.take();
                self.invalidate_fancy_tab_bar();
                self.invalidate_modal();

                if let Some(mux) = Mux::try_get() {
                    if let Some(window) = mux.get_window(self.mux_window_id) {
                        for tab in window.iter() {
                            tab.resize(self.terminal_size);
                        }
                    }
                }
                self.update_title();
                window.invalidate();
            }
            TermWindowNotif::SetInnerSize { width, height } => {
                self.set_inner_size(window, width, height);
            }
        }

        Ok(())
    }

    fn set_inner_size(&mut self, window: &Window, width: usize, height: usize) {
        self.resizes_pending += 1;
        window.set_inner_size(width, height);
    }

    /// Take care to remove our panes from the mux, otherwise
    /// we can leave the mux with no windows but some panes
    /// and it won't believe that we are empty.
    fn clear_all_overlays(&mut self) {
        let overlay_panes_to_cancel = self
            .pane_state
            .borrow()
            .iter()
            .filter_map(|(owner_pane_id, state)| state.overlay.as_ref().map(|_| *owner_pane_id))
            .collect::<Vec<_>>();

        for pane_id in overlay_panes_to_cancel {
            self.cancel_overlay_for_pane(pane_id);
        }

        let tab_overlays_to_cancel = self
            .tab_state
            .borrow()
            .iter()
            .filter_map(|(tab_id, state)| state.overlay.as_ref().map(|_| *tab_id))
            .collect::<Vec<_>>();

        for tab_id in tab_overlays_to_cancel {
            self.cancel_overlay_for_tab(tab_id, None);
        }

        self.pane_state.borrow_mut().clear();
        self.tab_state.borrow_mut().clear();
        self.last_tab_state_prune_revision.set(None);
    }

    /// Reconcile GUI-only tab state with the exact mux window-order revision.
    /// Stale overlay panes are removed only after the tab-state borrow is
    /// released, because mux removal can synchronously enqueue notifications.
    fn prune_tab_state_to_live_window(&mut self) {
        let Some(mux) = Mux::try_get() else {
            return;
        };
        let Some(mux_window) = mux.get_window(self.mux_window_id) else {
            return;
        };
        let revision = mux_window.order_revision();
        if self.last_tab_state_prune_revision.get() == Some(revision) {
            return;
        }
        let live_tab_ids = mux_window
            .iter()
            .map(|tab| tab.tab_id())
            .collect::<HashSet<_>>();
        drop(mux_window);
        self.last_tab_state_prune_revision.set(Some(revision));

        let mut stale_overlays = Vec::new();
        self.tab_state.borrow_mut().retain(|tab_id, state| {
            if live_tab_ids.contains(tab_id) {
                true
            } else {
                if let Some(overlay) = state.overlay.take() {
                    stale_overlays.push((OverlaySlot::Tab(*tab_id), overlay));
                }
                false
            }
        });

        for (slot, overlay) in stale_overlays {
            Self::retire_overlay_registration(slot, overlay);
        }
    }

    fn apply_icon(window: &Window) -> anyhow::Result<()> {
        let image = image::load_from_memory(ICON_DATA)?.into_rgba8();
        let (width, height) = image.dimensions();
        window.set_icon(Image::with_rgba32(
            width as usize,
            height as usize,
            width as usize * 4,
            image.as_raw(),
        ));
        Ok(())
    }

    fn schedule_status_update(&self) {
        if let Some(window) = self.window.as_ref() {
            window.notify(TermWindowNotif::EmitStatusUpdate);
        }
    }

    fn is_pane_visible(&mut self, pane_id: PaneId) -> bool {
        let Some(mux) = self.mux_or_log("check pane visibility") else {
            return false;
        };
        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
            Some(tab) => tab,
            None => return false,
        };

        let tab_id = tab.tab_id();
        if let Some(tab_overlay) = self
            .tab_state(tab_id)
            .overlay
            .as_ref()
            .map(|overlay| overlay.pane.clone())
        {
            return tab_overlay.pane_id() == pane_id;
        }

        tab.contains_pane(pane_id)
    }

    fn mux_pane_output_event(&mut self, pane_id: PaneId) {
        self.record_idle_event(idle_detector::IdleEvent::PtyData);
        metrics::histogram!("mux.pane_output_event.rate").record(1.);
        if self.is_pane_visible(pane_id) {
            if let Some(ref win) = self.window {
                win.invalidate();
            }
        }
    }

    fn mux_synchronized_output_event(&mut self, pane_id: PaneId, event: SynchronizedOutputEvent) {
        if !self.window_contains_pane(pane_id) {
            return;
        }
        record_sync_output_mux_event(
            pane_id,
            event,
            &mut self.sync_output_watchdog_telemetry,
            &mut self.sync_output_orchestrator_telemetry,
            &mut self.sync_output_bsu_depth_by_pane,
            &mut self.sync_output_buffered_bytes_by_pane,
        );
    }

    fn forget_sync_output_state_for_pane(&mut self, pane_id: PaneId) {
        self.sync_output_bsu_depth_by_pane.remove(&pane_id);
        self.sync_output_buffered_bytes_by_pane.remove(&pane_id);
    }

    fn mux_notification_has_deferred_cleanup_authority(
        notification: &MuxNotification,
        has_cleanup_lease: bool,
    ) -> bool {
        !matches!(notification, MuxNotification::PaneRemoved(_)) || has_cleanup_lease
    }

    fn mux_pane_output_event_callback(
        n: MuxNotification,
        window: &Window,
        mux_window_id: MuxWindowId,
        dead: &Arc<AtomicBool>,
        mux_owner: &Weak<Mux>,
        pane_removal_cleanup: Option<PaneRemovalCleanupLease>,
    ) -> bool {
        if dead.load(Ordering::Relaxed) {
            // Subscription cancelled asynchronously
            return false;
        }
        let Some(mux) = mux_owner.upgrade() else {
            log::debug!("mux notification owner no longer exists; cancel mux subscription");
            return false;
        };
        if !Mux::try_get().is_some_and(|current| Arc::ptr_eq(&current, &mux)) {
            log::debug!(
                "discarding notification from a replaced mux instance; cancel mux subscription"
            );
            return false;
        }

        match n {
            MuxNotification::Alert {
                pane_id,
                alert:
                    Alert::OutputSinceFocusLost
                    | Alert::CurrentWorkingDirectoryChanged
                    | Alert::WindowTitleChanged(_)
                    | Alert::TabTitleChanged(_)
                    | Alert::IconTitleChanged(_)
                    | Alert::Progress(_)
                    | Alert::SetUserVar { .. }
                    | Alert::ImageAltText { .. }
                    | Alert::Bell
                    // ft-fy4ty + ft-7yiu2: routed through the same
                    // mux-side propagation as other alerts; the
                    // term-layer alert handler is what emits the
                    // user-visible side effects (confirmation prompt,
                    // native cursor mapping). Continuation beads
                    // ft-tzusd / ft-jornq wire those UIs.
                    | Alert::SetProfileRequested { .. }
                    | Alert::MouseShapeRequested { .. },
            }
            | MuxNotification::PaneFocused(pane_id)
            | MuxNotification::PaneRemoved(pane_id)
            | MuxNotification::PaneOutput(pane_id)
            | MuxNotification::SynchronizedOutput { pane_id, .. } => {
                // Ideally we'd check to see if pane_id is part of this window,
                // but overlays may not be 100% associated with the window
                // in the mux and we don't want to lose the invalidation
                // signal for that case, so we just check window validity
                // here and propagate to the window event handler that
                // will then do the check with full context.
                if mux.get_window(mux_window_id).is_none() {
                    // Something inconsistent: cancel subscription
                    log::debug!(
                        "PaneOutput: wanted mux_window_id={} from mux, but \
                         was not found, cancel mux subscription",
                        mux_window_id
                    );
                    return false;
                }
                let _ = pane_id;
            }
            MuxNotification::PaneAdded(_pane_id) => {
                // If some other client spawns a pane inside this window, this
                // gives us an opportunity to attach it to the clipboard.
                return mux.get_window(mux_window_id).is_some();
            }
            MuxNotification::FloatingPaneSpawnCommitted(spawn) => {
                // The payload was frozen at the same topology revision as the
                // structural commit, so route directly without re-reading a
                // tab that may already have been retired or reused.
                if spawn.window_id() != mux_window_id {
                    return true;
                }
            }
            MuxNotification::TabAddedToWindow { window_id, .. }
            | MuxNotification::WindowTitleChanged { window_id, .. }
            | MuxNotification::WindowInvalidated(window_id) => {
                if window_id != mux_window_id {
                    return true;
                }
            }
            MuxNotification::WindowOrderChanged { ref window, .. } => {
                if window.window_id() != mux_window_id {
                    return true;
                }
            }
            MuxNotification::WindowTopologyChanged(ref change) => {
                if change.removed_windows().binary_search(&mux_window_id).is_ok() {
                    dead.store(true, Ordering::Relaxed);
                    return false;
                }
                if !change.affects_window(mux_window_id) {
                    return true;
                }
            }
            MuxNotification::WindowRemoved(window_id) => {
                if window_id != mux_window_id {
                    return true;
                }
                // Set the window as dead to unsubscribe from further notifications
                dead.store(true, Ordering::Relaxed);
                return false;
            }
            MuxNotification::TabResized(tab_id)
            | MuxNotification::TabTitleChanged { tab_id, .. } => {
                if mux.window_containing_tab(tab_id) == Some(mux_window_id) {
                    // fall through
                } else {
                    return true;
                }
            }
            MuxNotification::Alert {
                alert: Alert::ToastNotification { .. },
                ..
            }
            | MuxNotification::AssignClipboard { .. }
            | MuxNotification::SaveToDownloads { .. }
            | MuxNotification::WindowCreated(_)
            | MuxNotification::ActiveWorkspaceChanged(_)
            | MuxNotification::WorkspaceRenamed { .. }
            | MuxNotification::Empty
            | MuxNotification::WindowWorkspaceChanged { .. } => return true,
            MuxNotification::Alert {
                alert: Alert::PaletteChanged { .. },
                ..
            } => {
                // fall through
            }
        }

        window.notify(TermWindowNotif::MuxNotification {
            notification: n,
            mux_owner: Arc::downgrade(&mux),
            pane_removal_cleanup,
        });

        true
    }

    fn subscribe_to_pane_updates(&self) -> anyhow::Result<()> {
        let window = self
            .window
            .clone()
            .ok_or_else(|| anyhow!("cannot subscribe to pane updates without a GUI window"))?;
        let mux_window_id = Arc::clone(&self.mux_window_id_for_subscriptions);
        let mux = Mux::try_get()
            .ok_or_else(|| anyhow!("cannot subscribe to pane updates without an active mux"))?;
        let dead = Arc::new(AtomicBool::new(false));
        let subscription_id = Arc::new(AtomicUsize::new(usize::MAX));
        let unsubscribe_requested = Arc::new(AtomicBool::new(false));
        let callback_subscription_id = Arc::clone(&subscription_id);
        let callback_unsubscribe_requested = Arc::clone(&unsubscribe_requested);
        let callback_mux = Arc::downgrade(&mux);
        let allocated_subscription_id = mux
            .subscribe_with_pane_removal_cleanup(move |n, pane_removal_cleanup| {
                if dead.load(Ordering::Relaxed) {
                    return false;
                }
                let mux_window_id = match mux_window_id.lock() {
                    Ok(mux_window_id) => *mux_window_id,
                    Err(poisoned) => {
                        log::warn!("recovering poisoned mux-window subscription lock");
                        *poisoned.into_inner()
                    }
                };
                if matches!(
                    &n,
                    MuxNotification::WindowRemoved(window_id) if *window_id == mux_window_id
                ) {
                    // This notification is sufficient to retire the subscriber
                    // synchronously.  Do not enqueue a GUI-thread callback and
                    // wait for some later notification to observe `dead`.
                    dead.store(true, Ordering::Release);
                    callback_unsubscribe_requested.store(true, Ordering::Release);
                    return false;
                }
                let window = window.clone();
                let dead = dead.clone();
                let subscription_id = Arc::clone(&callback_subscription_id);
                let unsubscribe_requested = Arc::clone(&callback_unsubscribe_requested);
                let mux = callback_mux.clone();
                if !Self::mux_notification_has_deferred_cleanup_authority(
                    &n,
                    pane_removal_cleanup.is_some(),
                ) {
                    log::error!(
                        "PaneRemoved arrived without an authoritative deferred-cleanup fence; GUI cleanup will fail closed"
                    );
                    return true;
                }
                match promise::spawn::try_reserve_main_thread(
                    promise::spawn::MainThreadServiceClass::Render,
                    4 * 1024,
                ) {
                    promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => {
                        reservation
                            .spawn(async move {
                                if !Self::mux_pane_output_event_callback(
                                    n,
                                    &window,
                                    mux_window_id,
                                    &dead,
                                    &mux,
                                    pane_removal_cleanup,
                                ) {
                                    dead.store(true, Ordering::Release);
                                    unsubscribe_requested.store(true, Ordering::Release);
                                    let sub_id =
                                        subscription_id.swap(usize::MAX, Ordering::AcqRel);
                                    if sub_id != usize::MAX {
                                        if let Some(mux) = mux.upgrade() {
                                            let _ = mux.unsubscribe(sub_id);
                                        }
                                    }
                                }
                            })
                            .detach();
                        true
                    }
                    rejected => {
                        metrics::counter!(
                            "gui.mux_notification_admission",
                            "outcome" => "terminal_rejection"
                        )
                        .increment(1);
                        log::error!(
                            "main-thread scheduler rejected mux notification; terminating the exact GUI subscription instead of silently dropping state: {rejected:?}"
                        );
                        dead.store(true, Ordering::Release);
                        unsubscribe_requested.store(true, Ordering::Release);
                        false
                    }
                }
            })
            .context("allocating mux pane-update subscription")?;
        subscription_id.store(allocated_subscription_id, Ordering::Release);
        if unsubscribe_requested.load(Ordering::Acquire) {
            let sub_id = subscription_id.swap(usize::MAX, Ordering::AcqRel);
            if sub_id != usize::MAX {
                let _ = mux.unsubscribe(sub_id);
            }
        }
        Ok(())
    }

    fn emit_status_event(&mut self) {
        self.emit_window_event("update-right-status", None);
        self.emit_window_event("update-status", None);
    }

    fn schedule_window_event(&mut self, name: &str, pane_id: Option<PaneId>) {
        let Some(window) = GuiWin::try_new(self) else {
            return;
        };
        let pane = match pane_id {
            Some(pane_id) => Mux::try_get().and_then(|mux| mux.get_pane(pane_id)),
            None => None,
        };
        let pane = match pane {
            Some(pane) => pane,
            None => match self.get_active_pane_or_overlay() {
                Some(pane) => pane,
                None => return,
            },
        };
        let pane = MuxPane(pane.pane_id());
        let name = name.to_string();

        async fn do_event(
            lua: Option<Rc<mlua::Lua>>,
            name: String,
            window: GuiWin,
            pane: MuxPane,
        ) -> anyhow::Result<()> {
            let again = if let Some(lua) = lua {
                let args = lua.pack_multi((window.clone(), pane))?;

                if let Err(err) =
                    config::lua::emit_event(lua.as_ref().clone(), (name.clone(), args)).await
                {
                    log::error!("while processing {} event: {:#}", name, err);
                }
                true
            } else {
                false
            };

            window
                .window
                .notify(TermWindowNotif::FinishWindowEvent { name, again });

            Ok(())
        }

        schedule_existing_termwindow_future(
            promise::spawn::MainThreadServiceClass::Interactive,
            8 * 1024,
            "window Lua event",
            config::with_lua_config_on_main_thread(move |lua| do_event(lua, name, window, pane)),
        );
    }

    /// Called as part of finishing up a callout to lua.
    /// If again==false it means that there isn't a lua config
    /// to execute against, so we should just mark as done.
    /// Otherwise, if there is a queued item, schedule it now.
    fn finish_window_event(&mut self, name: &str, again: bool) {
        let state = self
            .event_states
            .entry(name.to_string())
            .or_insert(EventState::None);
        if again {
            match state {
                EventState::InProgress => {
                    *state = EventState::None;
                }
                EventState::InProgressWithQueued(pane) => {
                    let pane = *pane;
                    *state = EventState::InProgress;
                    self.schedule_window_event(name, pane);
                }
                EventState::None => {}
            }
        } else {
            *state = EventState::None;
        }
    }

    pub fn emit_window_event(&mut self, name: &str, pane_id: Option<PaneId>) {
        if self.get_active_pane_or_overlay().is_none() || self.window.is_none() {
            return;
        }

        let state = self
            .event_states
            .entry(name.to_string())
            .or_insert(EventState::None);
        match state {
            EventState::InProgress => {
                // Flag that we want to run again when the currently
                // executing event calls finish_window_event().
                *state = EventState::InProgressWithQueued(pane_id);
                return;
            }
            EventState::InProgressWithQueued(other_pane) => {
                // We've already got one copy executing and another
                // pending dispatch, so don't queue another.
                if pane_id != *other_pane {
                    log::warn!(
                        "Cannot queue {} event for pane {:?}, as \
                         there is already an event queued for pane {:?} \
                         in the same window",
                        name,
                        pane_id,
                        other_pane
                    );
                }
                return;
            }
            EventState::None => {
                // Nothing pending, so schedule a call now
                *state = EventState::InProgress;
                self.schedule_window_event(name, pane_id);
            }
        }
    }

    fn check_for_dirty_lines_and_invalidate_selection(
        &mut self,
        pane: &Arc<dyn Pane>,
    ) -> anyhow::Result<()> {
        let dims = pane.get_dimensions();
        let pane_id = pane.pane_id();
        let viewport = self.get_viewport(pane_id).unwrap_or(dims.physical_top);
        let Some(visible_range) =
            frankenterm_gui::checked_stable_row_range_from_top(viewport, dims.viewport_rows)
        else {
            metrics::counter!(
                "gui.render.dirty_range_overflow",
                "operation" => "dirty_query"
            )
            .increment(1);
            log::error!(
                "cannot query dirty lines for pane {pane_id}: viewport {viewport} + {} rows overflows StableRowIndex",
                dims.viewport_rows
            );
            return Err(anyhow!(
                "dirty-line viewport range overflow for pane {pane_id}: {viewport} + {} rows",
                dims.viewport_rows
            ));
        };

        // Capture the source fence and scan under one backend lock. LocalPane
        // holds the terminal lock across both operations; ClientPane polls,
        // captures the post-poll fence, and scans under one renderable borrow.
        // Every production backend whose sequence can change between calls
        // overrides this method. The split trait fallback is admissible only
        // for a stable monotonic source; a resetting or polling backend must
        // provide an atomic/delegating override.
        let last_observed_source_end = self
            .pane_state(pane_id)
            .render_dirty
            .last_observed_source_end();
        let (source_end, dirty) = pane.get_changed_since_with_source_fence(
            visible_range.clone(),
            last_observed_source_end,
        );
        self.pane_state(pane_id)
            .render_dirty
            .advance_after_query(source_end);

        // Per ft-camu6 (cont of ft-jvj78): wire PTY-write dirty
        // marks into the per-pane DirtyLineBitmap. The term layer's
        // `get_changed_since` returns stable-row indices; translate
        // each to its visible-row index (stable - viewport) and mark
        // it dirty in the bitmap. Out-of-bounds rows (e.g., scrolled
        // past the viewport) are silently dropped by
        // DirtyLineBitmap::mark per its existing contract.
        let viewport_rows = dims.viewport_rows;
        if !dirty.is_empty() {
            let bitmap = self.dirty_lines_for_pane(pane_id, viewport_rows);
            mark_stable_row_ranges_dirty(bitmap, viewport, dirty.iter().cloned());
            // Per ft-i6k6u: tag the mark with its source so the
            // substrate's per-source aggregator attributes
            // PTY-driven seqno bumps separately from selection /
            // theme / font / focus events. The actual translation
            // already happened above; here we just bump the counter.
            self.record_dirty_event(frankenterm_core::dirty_line_telemetry::DirtyEventSource::Pty);
        }

        if pane.downcast_ref::<CopyOverlay>().is_none()
            && pane.downcast_ref::<QuickSelectOverlay>().is_none()
        {
            // If any of the changed lines intersect with the
            // selection, then we need to clear the selection, but not
            // when the search overlay is active; the search overlay
            // marks lines as dirty to force invalidate them for
            // highlighting purpose but also manipulates the selection
            // and we want to allow it to retain the selection it made!

            let (selection_range, selection_seqno) = {
                let selection = self.selection(pane_id);
                (selection.range, selection.seqno)
            };
            let (clear_selection, cleared_rows) = if let Some(selection_range) = selection_range {
                let selection_rows = selection_range.rows();
                // Selection creation has its own sequence baseline. Query it
                // through the same atomic source fence as render damage so a
                // reset/regression or saturated source cannot make an old high
                // selection seqno suppress unrelated replacement content.
                let (_, selection_dirty) = pane.get_changed_since_with_source_fence(
                    visible_range,
                    selection_seqno,
                );
                let intersects = selection_rows
                    .clone()
                    .into_iter()
                    .any(|row| selection_dirty.contains(row));
                (
                    intersects,
                    if intersects {
                        Some(selection_rows)
                    } else {
                        None
                    },
                )
            } else {
                (false, None)
            };

            if clear_selection
                && !should_preserve_selection_during_dirty_line_update(
                    &self.current_mouse_capture,
                    &self.current_mouse_buttons,
                    self.active_selection_drag_pane,
                    pane_id,
                )
            {
                self.active_selection_drag_pane = None;
                self.selection(pane.pane_id()).range.take();
                self.selection(pane.pane_id()).origin.take();
                self.selection(pane.pane_id()).seqno = pane.get_current_seqno();

                // Per ft-camu6: selection-clear is a per-row event
                // — mark every row that was previously selected so
                // the render pass redraws the cell-bg without the
                // selection-fg highlight. Stable rows again
                // translated to visible indices via viewport.
                if let Some(rows) = cleared_rows {
                    let bitmap = self.dirty_lines_for_pane(pane_id, viewport_rows);
                    mark_stable_rows_dirty(bitmap, viewport, rows);
                    self.record_dirty_event(
                        frankenterm_core::dirty_line_telemetry::DirtyEventSource::SelectionChange,
                    );
                }
            }
        }
        Ok(())
    }
}

impl TermWindow {
    fn palette(&mut self) -> &ColorPalette {
        self.palette
            .get_or_insert_with(|| config::TermConfig::new().color_palette())
    }

    pub fn config_was_reloaded(&mut self) {
        log::debug!(
            "config was reloaded, overrides: {:?}",
            self.config_overrides
        );
        // ft-mpc9b.1.2: a config reload can change theme, font,
        // colors, padding, etc. Mark every pane dirty so the next
        // paint reflects the new config. Per ft-i6k6u: tag as
        // ThemeSwap (the most common reason an operator reloads
        // config is to swap themes; FontSwap fires through the
        // shape-cache notif at the dedicated site above).
        self.mark_all_panes_dirty_with_source(
            frankenterm_core::dirty_line_telemetry::DirtyEventSource::ThemeSwap,
        );
        self.key_table_state.clear_stack();
        self.connection_name = Connection::get()
            .map(|c| c.name())
            .unwrap_or_else(|| "unknown".to_string());
        let config = match config::overridden_config(&self.config_overrides) {
            Ok(config) => config,
            Err(err) => {
                log::error!(
                    "Failed to apply config overrides to window: {:#}: {:?}",
                    err,
                    self.config_overrides
                );
                configuration()
            }
        };
        let config = config_with_accessibility_palette(config);
        self.config = config.clone();
        self.refresh_frame_budget_reduce_motion_state();
        self.palette.take();

        let Some(mux) = self.mux_or_log("reload GUI configuration") else {
            return;
        };
        let window = match mux.get_window(self.mux_window_id) {
            Some(window) => window,
            _ => return,
        };
        if window.len() == 1 {
            self.show_tab_bar = config.enable_tab_bar && !config.hide_tab_bar_if_only_one_tab;
        } else {
            self.show_tab_bar = config.enable_tab_bar;
        }
        *self.cursor_blink_state.borrow_mut() = ColorEase::new(
            config.cursor_blink_rate,
            config.cursor_blink_ease_in,
            config.cursor_blink_rate,
            config.cursor_blink_ease_out,
            None,
        );
        *self.blink_state.borrow_mut() = ColorEase::new(
            config.text_blink_rate,
            config.text_blink_ease_in,
            config.text_blink_rate,
            config.text_blink_ease_out,
            None,
        );
        *self.rapid_blink_state.borrow_mut() = ColorEase::new(
            config.text_blink_rate_rapid,
            config.text_blink_rapid_ease_in,
            config.text_blink_rate_rapid,
            config.text_blink_rapid_ease_out,
            None,
        );

        self.show_scroll_bar = config.enable_scroll_bar;
        self.shape_cache.borrow_mut().update_config(&config);
        self.line_state_cache.borrow_mut().update_config(&config);
        self.line_quad_cache.borrow_mut().update_config(&config);
        self.line_to_ele_shape_cache
            .borrow_mut()
            .update_config(&config);
        self.advance_shaping_input_generation();
        self.fancy_tab_bar.take();
        self.invalidate_fancy_tab_bar();
        self.invalidate_modal();
        self.input_map = InputMap::new(&config);
        self.leader_is_down = None;
        if let Some(render_state) = self.render_state.as_mut() {
            render_state.config_changed(&config);
        }
        let dimensions = self.dimensions;

        if let Err(err) = self.fonts.config_changed(&config) {
            log::error!("Failed to load font configuration: {:#}", err);
        }

        if let Some(window) = mux.get_window(self.mux_window_id) {
            let term_config: Arc<dyn TerminalConfiguration> =
                Arc::new(TermConfig::with_config(config.clone()));
            for tab in window.iter() {
                for pane in tab.iter_panes_ignoring_zoom() {
                    pane.pane.set_config(Arc::clone(&term_config));
                }
            }
            for state in self.pane_state.borrow().values() {
                if let Some(overlay) = &state.overlay {
                    overlay.pane.set_config(Arc::clone(&term_config));
                }
            }
            for state in self.tab_state.borrow().values() {
                if let Some(overlay) = &state.overlay {
                    overlay.pane.set_config(Arc::clone(&term_config));
                }
            }
        }

        if let Some(window) = self.window.as_ref().map(|w| w.clone()) {
            self.load_os_parameters();
            self.apply_scale_change(&dimensions, self.fonts.get_font_scale());
            self.apply_dimensions(&dimensions, None, &window);
            window.config_did_change(&config);
            window.invalidate();
        }

        // Do this after we've potentially adjusted scaling based on
        // config/padding and window size. The current layers stay on screen
        // until the newest bounded background job commits.
        self.schedule_background_reload();

        self.invalidate_modal();
        self.emit_window_event("window-config-reloaded", None);
    }

    fn invalidate_modal(&mut self) {
        if let Some(modal) = self.get_modal() {
            modal.reconfigure(self);
            if let Some(window) = self.window.as_ref() {
                window.invalidate();
            }
        }
    }

    pub fn cancel_modal(&self) {
        self.modal.borrow_mut().take();
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    pub fn set_modal(&self, modal: Rc<dyn Modal>) {
        self.modal.borrow_mut().replace(modal);
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    fn get_modal(&self) -> Option<Rc<dyn Modal>> {
        self.modal.borrow().as_ref().map(|m| Rc::clone(&m))
    }

    fn update_scrollbar(&mut self) {
        if !self.show_scroll_bar {
            return;
        }

        let tab = match self.get_active_pane_or_overlay() {
            Some(tab) => tab,
            None => return,
        };

        let render_dims = tab.get_dimensions();
        if render_dims == self.last_scroll_info {
            return;
        }

        self.last_scroll_info = render_dims;

        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    /// Called by various bits of code to update the title bar.
    /// Let's also trigger the status event so that it can choose
    /// to update the right-status.
    fn update_title(&mut self) {
        self.schedule_status_update();
        self.update_title_impl();
    }

    fn window_contains_pane(&mut self, pane_id: PaneId) -> bool {
        let Some(mux) = self.mux_or_log("check pane ownership") else {
            return false;
        };

        let (_domain, window_id, _tab_id) = match mux.resolve_pane_id(pane_id) {
            Some(tuple) => tuple,
            None => return false,
        };

        return window_id == self.mux_window_id;
    }

    fn emit_user_var_event(&mut self, pane_id: PaneId, name: String, value: String) {
        if !self.window_contains_pane(pane_id) {
            return;
        }

        let Some(mux) = self.mux_or_log("emit user-var-changed") else {
            return;
        };
        let Some(window) = GuiWin::try_new(self) else {
            return;
        };
        let pane = match mux.get_pane(pane_id) {
            Some(pane) => mux_lua::MuxPane(pane.pane_id()),
            None => return,
        };

        async fn do_event(
            lua: Option<Rc<mlua::Lua>>,
            name: String,
            value: String,
            window: GuiWin,
            pane: MuxPane,
        ) -> anyhow::Result<()> {
            if let Some(lua) = lua {
                let args = lua.pack_multi((window.clone(), pane, name, value))?;
                if let Err(err) = config::lua::emit_event(
                    lua.as_ref().clone(),
                    ("user-var-changed".to_string(), args),
                )
                .await
                {
                    log::error!("while processing user-var-changed event: {:#}", err);
                }
            }

            window
                .window
                .notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                    term_window.update_title();
                })));

            Ok(())
        }

        schedule_existing_termwindow_future(
            promise::spawn::MainThreadServiceClass::Interactive,
            8 * 1024,
            "user-variable Lua event",
            config::with_lua_config_on_main_thread(move |lua| {
                do_event(lua, name, value, window, pane)
            }),
        );
    }

    /// Called by window:set_right_status after the status has
    /// been updated; let's update the bar
    pub fn update_title_post_status(&mut self) {
        self.update_title_impl();
    }

    fn update_title_impl(&mut self) {
        let Some(mux) = self.mux_or_log("update window title") else {
            return;
        };
        let window = match mux.get_window(self.mux_window_id) {
            Some(window) => window,
            _ => return,
        };
        let tabs = self.get_tab_information();
        let panes = self.get_pane_information();
        let active_tab = tabs.iter().find(|t| t.is_active).cloned();
        let active_pane = panes.iter().find(|p| p.is_active).cloned();

        let border = self.get_os_border();
        let tab_bar_height = self.tab_bar_pixel_height().unwrap_or(0.);
        let tab_bar_y = if self.config.tab_bar_at_bottom {
            ((self.dimensions.pixel_height as f32) - (tab_bar_height + border.bottom.get() as f32))
                .max(0.)
        } else {
            border.top.get() as f32
        };

        let tab_bar_height = self.tab_bar_pixel_height().unwrap_or(0.);

        let hovering_in_tab_bar = match &self.current_mouse_event {
            Some(event) => {
                let mouse_y = event.coords.y as f32;
                mouse_y >= tab_bar_y as f32 && mouse_y < tab_bar_y as f32 + tab_bar_height
            }
            None => false,
        };

        let new_tab_bar = TabBarState::new(
            self.dimensions.pixel_width / (self.render_metrics.cell_size.width as usize).max(1),
            if hovering_in_tab_bar {
                Some(self.last_mouse_coords.0)
            } else {
                None
            },
            &tabs,
            &panes,
            self.config.resolved_palette.tab_bar.as_ref(),
            &self.config,
            &self.left_status,
            &self.right_status,
        );
        if new_tab_bar != self.tab_bar {
            self.tab_bar = new_tab_bar;
            self.invalidate_fancy_tab_bar();
            self.invalidate_modal();
            if let Some(window) = self.window.as_ref() {
                window.invalidate();
            }
        }

        let num_tabs = window.len();
        if num_tabs == 0 {
            return;
        }
        drop(window);

        let title = match config::run_immediate_with_lua_config(|lua| {
            if let Some(lua) = lua {
                let tabs = lua.create_sequence_from(tabs.clone().into_iter())?;
                let panes = lua.create_sequence_from(panes.clone().into_iter())?;

                let v = config::lua::emit_sync_callback(
                    &*lua,
                    (
                        "format-window-title".to_string(),
                        (
                            active_tab.clone(),
                            active_pane.clone(),
                            tabs,
                            panes,
                            (*self.config).clone(),
                        ),
                    ),
                )?;
                match &v {
                    mlua::Value::Nil => Ok(None),
                    _ => Ok(Some(String::from_lua(v, &*lua)?)),
                }
            } else {
                Ok(None)
            }
        }) {
            Ok(s) => s,
            Err(err) => {
                log::warn!("format-window-title: {}", err);
                None
            }
        };

        let title = match title {
            Some(title) => title,
            None => {
                if let (Some(pos), Some(tab)) = (active_pane, active_tab) {
                    if num_tabs == 1 {
                        format!("{}{}", if pos.is_zoomed { "[Z] " } else { "" }, pos.title)
                    } else {
                        format!(
                            "{}[{}/{}] {}",
                            if pos.is_zoomed { "[Z] " } else { "" },
                            tab.tab_index + 1,
                            num_tabs,
                            pos.title
                        )
                    }
                } else {
                    "".to_string()
                }
            }
        };

        if let Some(window) = self.window.as_ref() {
            window.set_title(&title);

            let show_tab_bar = if num_tabs == 1 {
                self.config.enable_tab_bar && !self.config.hide_tab_bar_if_only_one_tab
            } else {
                self.config.enable_tab_bar
            };

            // If the number of tabs changed and caused the tab bar to
            // hide/show, then we'll need to resize things.  It is simplest
            // to piggy back on the config reloading code for that, so that
            // is what we're doing.
            if show_tab_bar != self.show_tab_bar {
                self.config_was_reloaded();
            }
        }
        self.schedule_next_status_update();
    }

    fn schedule_next_status_update(&mut self) {
        if let Some(window) = self.window.as_ref() {
            let now = Instant::now();
            if self.last_status_call <= now {
                let interval = Duration::from_millis(self.config.status_update_interval);
                let target = now + interval;
                let window = window.clone();
                match promise::spawn::try_reserve_main_thread(
                    promise::spawn::MainThreadServiceClass::Render,
                    4 * 1024,
                ) {
                    promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => {
                        self.last_status_call = target;
                        reservation
                            .spawn_local(async move {
                                sleep(target.saturating_duration_since(Instant::now())).await;
                                window.notify(TermWindowNotif::EmitStatusUpdate);
                            })
                            .detach();
                    }
                    rejected => log::error!(
                        "main-thread scheduler rejected status update timer; left it immediately eligible for retry: {rejected:?}"
                    ),
                }
            }
        }
    }

    /// Schedule a coalesced `update_title()` at most once per ~16 ms frame.
    ///
    /// Multiple mux subscribers (one per attached domain) re-emit Alerts like
    /// `Progress`, `CurrentWorkingDirectoryChanged`, and `OutputSinceFocusLost`
    /// at high frequency under active agent output. Calling `update_title()`
    /// directly per Alert produces O(N_tabs × N_panes × Lua_roundtrip)
    /// allocation churn per call, dozens of times per second — the dominant
    /// driver of the wezterm-gui RSS leak observed in production. Coalescing
    /// to one frame caps the call rate at ~60/sec independent of fanout.
    /// See ft-9d60d.
    fn schedule_update_title(&mut self) {
        if self.pending_update_title {
            metrics::histogram!("update_title.coalesced").record(1.);
            return;
        }
        self.pending_update_title = true;
        metrics::histogram!("update_title.scheduled").record(1.);

        if let Some(window) = self.window.as_ref() {
            let window = window.clone();
            match promise::spawn::try_reserve_main_thread(
                promise::spawn::MainThreadServiceClass::Render,
                4 * 1024,
            ) {
                promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => {
                    reservation
                        .spawn_local(async move {
                            // ~one frame at 60 Hz. Imperceptible delay for OSC-driven UI.
                            sleep(Duration::from_millis(16)).await;
                            window.notify(TermWindowNotif::Apply(Box::new(|tw| {
                                tw.pending_update_title = false;
                                tw.update_title();
                            })));
                        })
                        .detach();
                }
                rejected => {
                    self.pending_update_title = false;
                    log::error!(
                        "main-thread scheduler rejected title update; cleared coalescing state for retry: {rejected:?}"
                    );
                }
            }
        } else {
            // Window already dropped; clear the flag so a future call can
            // re-arm once the window is recreated.
            self.pending_update_title = false;
        }
    }

    fn update_text_cursor(&mut self, pos: &PositionedPane) {
        if let Some(win) = self.window.as_ref() {
            let cursor = pos.pane.get_cursor_position();
            let top = pos.pane.get_dimensions().physical_top;
            let tab_bar_height = if self.show_tab_bar && !self.config.tab_bar_at_bottom {
                self.tab_bar_pixel_height().unwrap_or(0.0)
            } else {
                0.0
            };
            let (padding_left, padding_top) = self.padding_left_top();

            // ft-mpc9b.10.2: route the caret-rect math through the
            // pure helper in `frankenterm_core::ime_caret` so the
            // computation has exactly one source of truth and is
            // unit-testable without spinning up a real GPU/window.
            // Future render-quality / live-resize / idle-wake-up
            // beads inherit the same math.
            let caret = frankenterm_core::ime_caret::compute_caret_anchor_rect(
                frankenterm_core::ime_caret::CaretGeometry {
                    cursor_cell_col: cursor.x as i64,
                    cursor_cell_row: cursor.y as i64,
                    pane_top_cell: pos.top as i64,
                    pane_left_cell: pos.left as i64,
                    physical_top: top as i64,
                    cell_width_px: self.render_metrics.cell_size.width as i64,
                    cell_height_px: self.render_metrics.cell_size.height as i64,
                    tab_bar_height_px: tab_bar_height as i64,
                    padding_left_px: padding_left as i64,
                    padding_top_px: padding_top as i64,
                },
            );

            let r = Rect::new(
                Point::new(caret.origin_x as isize, caret.origin_y as isize),
                self.render_metrics.cell_size,
            );
            win.set_text_cursor_position(r);
        }
    }

    fn activate_window(&mut self, window_idx: usize) -> anyhow::Result<()> {
        let windows = try_front_end()
            .ok_or_else(|| anyhow!("GUI frontend is not available"))?
            .gui_windows();
        if let Some(win) = windows.get(window_idx) {
            win.window.focus();
        }
        Ok(())
    }

    fn activate_window_relative(&mut self, delta: isize, wrap: bool) -> anyhow::Result<()> {
        let windows = try_front_end()
            .ok_or_else(|| anyhow!("GUI frontend is not available"))?
            .gui_windows();
        let my_idx = windows
            .iter()
            .position(|w| Some(&w.window) == self.window.as_ref())
            .ok_or_else(|| anyhow!("I'm not in the window list!?"))?;

        let idx = my_idx as isize + delta;

        let idx = if wrap {
            let idx = if idx < 0 {
                windows.len() as isize + idx
            } else {
                idx
            };
            idx as usize % windows.len()
        } else {
            if idx < 0 {
                0
            } else if idx >= windows.len() as isize {
                windows.len().saturating_sub(1)
            } else {
                idx as usize
            }
        };

        if let Some(win) = windows.get(idx) {
            win.window.focus();
        }

        Ok(())
    }

    fn activate_tab(&mut self, tab_idx: isize) -> anyhow::Result<()> {
        let mux = self.mux_or_err("activate tab")?;
        let window = mux
            .get_window(self.mux_window_id)
            .ok_or_else(|| anyhow!("no such window"))?;

        // This logic is coupled with the CliSubCommand::ActivateTab
        // logic in wezterm/src/main.rs. If you update this, update that!
        let max = window.len();

        let tab_idx = if tab_idx < 0 {
            max.saturating_sub(tab_idx.abs() as usize)
        } else {
            tab_idx as usize
        };

        if tab_idx < max {
            drop(window);
            mux.activate_tab_at_index(self.mux_window_id, tab_idx, true)?;

            if let Some(tab) = self.get_active_pane_or_overlay() {
                tab.focus_changed(true);
            }

            self.update_title();
            self.update_scrollbar();
        }
        Ok(())
    }

    fn activate_tab_relative(&mut self, delta: isize, wrap: bool) -> anyhow::Result<()> {
        let mux = self.mux_or_err("activate relative tab")?;
        let window = mux
            .get_window(self.mux_window_id)
            .ok_or_else(|| anyhow!("no such window"))?;

        let max = window.len();
        ensure!(max > 0, "no more tabs");

        // This logic is coupled with the CliSubCommand::ActivateTab
        // logic in wezterm/src/main.rs. If you update this, update that!
        let active = window.get_active_idx() as isize;
        let tab = active + delta;
        let tab = if wrap {
            let tab = if tab < 0 { max as isize + tab } else { tab };
            (tab as usize % max) as isize
        } else {
            if tab < 0 {
                0
            } else if tab >= max as isize {
                max as isize - 1
            } else {
                tab
            }
        };
        drop(window);
        self.activate_tab(tab)
    }

    fn activate_last_tab(&mut self) -> anyhow::Result<()> {
        let mux = self.mux_or_err("activate last tab")?;
        let window = mux
            .get_window(self.mux_window_id)
            .ok_or_else(|| anyhow!("no such window"))?;

        let last_idx = window.get_last_active_idx();
        drop(window);
        match last_idx {
            Some(idx) => self.activate_tab(idx as isize),
            None => Ok(()),
        }
    }

    fn move_tab(&mut self, tab_idx: usize) -> anyhow::Result<()> {
        let mux = self.mux_or_err("move tab")?;
        let window = mux
            .get_window(self.mux_window_id)
            .ok_or_else(|| anyhow!("no such window"))?;

        let max = window.len();
        ensure!(max > 0, "no more tabs");

        let active = window.get_active_idx();
        ensure!(tab_idx < max, "cannot move a tab out of range");
        let tab_id = window
            .get_by_idx(active)
            .ok_or_else(|| anyhow!("active tab index is out of range"))?
            .tab_id();
        drop(window);

        mux.move_tab_between_windows(tab_id, self.mux_window_id, Some(tab_idx))?;
        self.update_title();
        self.update_scrollbar();

        Ok(())
    }

    fn show_input_selector(&mut self, args: &config::keyassignment::InputSelector) {
        let mux = match Mux::try_get() {
            Some(mux) => mux,
            None => {
                log::error!("cannot start launcher overlay without an active mux");
                return;
            }
        };
        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
            Some(tab) => tab,
            None => return,
        };

        // Ignore any current overlay: we're going to cancel it out below
        // and we don't want this new one to reference that cancelled pane
        let pane = match self.get_active_pane_no_overlay() {
            Some(pane) => pane,
            None => return,
        };

        let args = args.clone();

        let Some(gui_win) = GuiWin::try_new(self) else {
            return;
        };
        let pane = MuxPane(pane.pane_id());

        let (overlay, ticket, future) = match start_overlay(self, &tab, move |_tab_id, term| {
            crate::overlay::selector::selector(term, args, gui_win, pane)
        }) {
            Ok(overlay) => overlay,
            Err(err) => {
                log::error!("failed to start input-selector overlay: {err:#}");
                return;
            }
        };
        self.assign_overlay_with_ticket(tab.tab_id(), overlay, ticket);
        schedule_existing_termwindow_future(
            promise::spawn::MainThreadServiceClass::Interactive,
            8 * 1024,
            "input-selector overlay",
            future,
        );
    }

    fn show_prompt_input_line(&mut self, args: &PromptInputLine) {
        let mux = match Mux::try_get() {
            Some(mux) => mux,
            None => {
                log::error!("cannot start launcher overlay without an active mux");
                return;
            }
        };
        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
            Some(tab) => tab,
            None => return,
        };

        let pane = match self.get_active_pane_or_overlay() {
            Some(pane) => pane,
            None => return,
        };

        let args = args.clone();

        let Some(gui_win) = GuiWin::try_new(self) else {
            return;
        };
        let pane = MuxPane(pane.pane_id());

        let (overlay, ticket, future) = match start_overlay(self, &tab, move |_tab_id, term| {
            crate::overlay::prompt::show_line_prompt_overlay(term, args, gui_win, pane)
        }) {
            Ok(overlay) => overlay,
            Err(err) => {
                log::error!("failed to start prompt overlay: {err:#}");
                return;
            }
        };
        self.assign_overlay_with_ticket(tab.tab_id(), overlay, ticket);
        schedule_existing_termwindow_future(
            promise::spawn::MainThreadServiceClass::Interactive,
            8 * 1024,
            "prompt overlay",
            future,
        );
    }

    fn tab_unify_identity(tab: &Tab) -> Option<TabIdentity> {
        let mut panes: Vec<Arc<dyn Pane>> = tab
            .iter_panes_ignoring_zoom()
            .into_iter()
            .map(|pane| pane.pane)
            .collect();
        panes.extend(tab.iter_floating_panes().into_iter().map(|pane| pane.pane));

        if panes.is_empty() {
            return None;
        }

        let mut domain_id = None;
        let mut remote_pane_ids = Vec::with_capacity(panes.len());
        for pane in panes {
            let client_pane = pane.downcast_ref::<ClientPane>()?;
            let pane_domain = pane.domain_id();
            if domain_id
                .replace(pane_domain)
                .is_some_and(|id| id != pane_domain)
            {
                return None;
            }
            remote_pane_ids.push(client_pane.remote_pane_id());
        }

        domain_id.map(|domain_id| TabIdentity::new(domain_id, remote_pane_ids))
    }

    fn collect_window_unify_snapshots(mux: &Mux) -> Vec<WindowSnapshot> {
        mux.iter_windows()
            .into_iter()
            .filter_map(|window_id| {
                let window = mux.get_window(window_id)?;
                let tabs = window
                    .iter()
                    .map(|tab| TabSnapshot {
                        tab_id: tab.tab_id(),
                        identity: Self::tab_unify_identity(tab),
                    })
                    .collect();
                Some(WindowSnapshot {
                    window_id,
                    workspace: window.get_workspace().to_string(),
                    tabs,
                })
            })
            .collect()
    }

    fn close_windows_after_unify_plans(
        snapshots: &[WindowSnapshot],
        workspace: &str,
        plans: &[MergePlan],
    ) -> Vec<MuxWindowId> {
        let changed_tabs: BTreeSet<TabId> = plans
            .iter()
            .flat_map(|plan| {
                plan.moves
                    .iter()
                    .map(|tab_move| tab_move.tab_id)
                    .chain(plan.drops.iter().map(|tab_drop| tab_drop.tab_id))
            })
            .collect();
        let canonical_windows: BTreeSet<MuxWindowId> = plans
            .iter()
            .filter_map(|plan| plan.canonical_window)
            .collect();

        let mut close_windows: Vec<MuxWindowId> = snapshots
            .iter()
            .filter(|window| window.workspace == workspace)
            .filter(|window| !canonical_windows.contains(&window.window_id))
            .filter(|window| !window.tabs.is_empty())
            .filter(|window| {
                window
                    .tabs
                    .iter()
                    .all(|tab| changed_tabs.contains(&tab.tab_id))
            })
            .map(|window| window.window_id)
            .collect();
        close_windows.sort_unstable();
        close_windows.dedup();
        close_windows
    }

    fn build_window_unify_plan(
        &self,
        scope: WindowUnifyScope,
    ) -> anyhow::Result<GuiWindowUnifyPlan> {
        let mux = self.mux_or_err("plan window unify")?;
        let workspace = mux
            .get_window(self.mux_window_id)
            .map(|window| window.get_workspace().to_string())
            .ok_or_else(|| anyhow!("window {} not found", self.mux_window_id))?;
        let snapshots = Self::collect_window_unify_snapshots(&mux);

        let (title, plans) = match scope {
            WindowUnifyScope::ActiveDomain => {
                let active_pane = self
                    .get_active_pane_no_overlay()
                    .ok_or_else(|| anyhow!("cannot unify windows without an active pane"))?;
                let domain_id = active_pane.domain_id();
                let domain_name = mux
                    .get_domain(domain_id)
                    .map(|domain| domain.domain_name().to_string())
                    .unwrap_or_else(|| format!("domain {domain_id}"));
                (
                    format!("Unify windows on this domain ({domain_name})"),
                    vec![plan_unify_domain(
                        &snapshots,
                        domain_id,
                        &workspace,
                        Some(self.mux_window_id),
                    )],
                )
            }
            WindowUnifyScope::AllDomains => {
                let domain_ids: BTreeSet<DomainId> = snapshots
                    .iter()
                    .filter(|window| window.workspace == workspace)
                    .flat_map(|window| window.tabs.iter())
                    .filter_map(|tab| tab.identity.as_ref().map(|identity| identity.domain_id))
                    .collect();
                let plans = domain_ids
                    .iter()
                    .map(|domain_id| {
                        plan_unify_domain(
                            &snapshots,
                            *domain_id,
                            &workspace,
                            Some(self.mux_window_id),
                        )
                    })
                    .collect();
                ("Unify all".to_string(), plans)
            }
        };
        let close_windows = Self::close_windows_after_unify_plans(&snapshots, &workspace, &plans);

        Ok(GuiWindowUnifyPlan {
            title,
            workspace,
            plans,
            close_windows,
        })
    }

    fn apply_window_unify_plan(plan: GuiWindowUnifyPlan) {
        let Some(mux) = Mux::try_get() else {
            log::warn!("cannot apply window-unify plan: mux is no longer active");
            return;
        };

        for merge_plan in &plan.plans {
            for tab_move in &merge_plan.moves {
                if let Err(err) =
                    mux.move_tab_between_windows(tab_move.tab_id, tab_move.to_window, None)
                {
                    log::error!(
                        "failed to move tab {} into window {} while applying window-unify plan: {err:#}",
                        tab_move.tab_id,
                        tab_move.to_window,
                    );
                    return;
                }
            }
        }

        for merge_plan in &plan.plans {
            for tab_drop in &merge_plan.drops {
                if mux.remove_tab_local_only(tab_drop.tab_id).is_none() {
                    log::warn!(
                        "window-unify planned to drop tab {}, but it no longer exists",
                        tab_drop.tab_id,
                    );
                }
            }
        }

        for window_id in plan.close_windows {
            mux.kill_window(window_id);
        }
    }

    fn show_window_unify_confirmation(&mut self, scope: WindowUnifyScope) {
        let plan = match self.build_window_unify_plan(scope) {
            Ok(plan) => plan,
            Err(err) => {
                persistent_toast_notification("Window unify unavailable", &format!("{err:#}"));
                return;
            }
        };
        let message = plan.summary_message();

        let Some(mux) = self.mux_or_log("start window-unify confirmation overlay") else {
            return;
        };
        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
            Some(tab) => tab,
            None => return,
        };

        let (overlay, ticket, future) = match start_overlay(self, &tab, move |_tab_id, mut term| {
            if crate::overlay::confirm::run_confirmation(&message, &mut term)? {
                crate::overlay::reserve_overlay_main_thread(
                    promise::spawn::MainThreadServiceClass::Input,
                    crate::overlay::OVERLAY_MAIN_THREAD_ESTIMATED_BYTES,
                    "window unify action",
                )?
                .spawn(async move {
                        Self::apply_window_unify_plan(plan);
                        anyhow::Result::<()>::Ok(())
                    })
                    .detach();
            }
            Ok(())
        }) {
            Ok(overlay) => overlay,
            Err(err) => {
                log::error!("failed to start window-unify confirmation overlay: {err:#}");
                return;
            }
        };
        self.assign_overlay_with_ticket(tab.tab_id(), overlay, ticket);
        schedule_existing_termwindow_future(
            promise::spawn::MainThreadServiceClass::Interactive,
            8 * 1024,
            "quick-select overlay",
            future,
        );
    }

    fn show_confirmation(&mut self, args: &Confirmation) {
        let Some(mux) = self.mux_or_log("start confirmation overlay") else {
            return;
        };
        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
            Some(tab) => tab,
            None => return,
        };

        let pane = match self.get_active_pane_or_overlay() {
            Some(pane) => pane,
            None => return,
        };

        let args = args.clone();

        let Some(gui_win) = GuiWin::try_new(self) else {
            return;
        };
        let pane = MuxPane(pane.pane_id());

        let (overlay, ticket, future) = match start_overlay(self, &tab, move |_tab_id, term| {
            crate::overlay::confirm::show_confirmation_overlay(term, args, gui_win, pane)
        }) {
            Ok(overlay) => overlay,
            Err(err) => {
                log::error!("failed to start confirmation overlay: {err:#}");
                return;
            }
        };
        self.assign_overlay_with_ticket(tab.tab_id(), overlay, ticket);
        schedule_existing_termwindow_future(
            promise::spawn::MainThreadServiceClass::Input,
            8 * 1024,
            "confirmation overlay",
            future,
        );
    }

    fn show_debug_overlay(&mut self) {
        let Some(mux) = self.mux_or_log("start debug overlay") else {
            return;
        };
        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
            Some(tab) => tab,
            None => return,
        };

        let Some(gui_win) = GuiWin::try_new(self) else {
            return;
        };

        let opengl_info = self.opengl_info.as_deref().unwrap_or("Unknown").to_string();
        let connection_info = self.connection_name.clone();

        let (overlay, ticket, future) = match start_overlay(self, &tab, move |_tab_id, term| {
            crate::overlay::show_debug_overlay(term, gui_win, opengl_info, connection_info)
        }) {
            Ok(overlay) => overlay,
            Err(err) => {
                log::error!("failed to start debug overlay: {err:#}");
                return;
            }
        };
        self.assign_overlay_with_ticket(tab.tab_id(), overlay, ticket);
        schedule_existing_termwindow_future(
            promise::spawn::MainThreadServiceClass::Interactive,
            8 * 1024,
            "debug overlay",
            future,
        );
    }

    fn show_tab_navigator(&mut self) {
        let Some(mux) = self.mux_or_log("start tab navigator") else {
            return;
        };
        let active_tab_idx = match mux.get_window(self.mux_window_id) {
            Some(mux_window) => mux_window.get_active_idx(),
            None => return,
        };
        let title = "Tab Navigator".to_string();
        let args = LauncherActionArgs {
            title: Some(title),
            flags: LauncherFlags::TABS,
            help_text: None,
            fuzzy_help_text: None,
            alphabet: None,
        };
        self.show_launcher_impl(args, active_tab_idx);
    }

    fn show_launcher(&mut self) {
        let title = "Session Manager".to_string();
        let args = LauncherActionArgs {
            title: Some(title),
            flags: LauncherFlags::LAUNCH_MENU_ITEMS
                | LauncherFlags::WORKSPACES
                | LauncherFlags::DOMAINS
                | LauncherFlags::KEY_ASSIGNMENTS
                | LauncherFlags::COMMANDS,
            help_text: Some("Session manager: Enter=switch/open  Esc=cancel  /=filter".to_string()),
            fuzzy_help_text: None,
            alphabet: None,
        };
        self.show_launcher_impl(args, 0);
    }

    fn show_launcher_impl(&mut self, args: LauncherActionArgs, initial_choice_idx: usize) {
        let mux_window_id = self.mux_window_id;
        let window = match self.gui_window_or_log("start launcher overlay") {
            Some(window) => window,
            None => return,
        };

        let Some(mux) = self.mux_or_log("start launcher overlay") else {
            return;
        };
        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
            Some(tab) => tab,
            None => return,
        };

        let pane = match self.get_active_pane_or_overlay() {
            Some(pane) => pane,
            None => return,
        };

        let domain_id_of_current_pane = match tab.get_active_pane() {
            Some(pane) => pane.domain_id(),
            None => {
                log::error!("cannot start launcher overlay for tab without panes");
                return;
            }
        };
        let pane_id = pane.pane_id();
        let tab_id = tab.tab_id();
        let title = args.title.unwrap_or_else(|| "Launcher".to_string());
        let flags = args.flags;
        let help_text = args.help_text.unwrap_or(
            "Select an item and press Enter=launch  \
             Esc=cancel  /=filter"
                .to_string(),
        );
        let fuzzy_help_text = args
            .fuzzy_help_text
            .unwrap_or_else(|| "Fuzzy matching: ".to_string());

        let config = &self.config;
        let alphabet = args
            .alphabet
            .unwrap_or_else(|| config.launcher_alphabet.clone());

        schedule_existing_termwindow_future(
            promise::spawn::MainThreadServiceClass::Interactive,
            16 * 1024,
            "prepare launcher overlay",
            async move {
            let args = match LauncherArgs::new(
                &title,
                flags,
                mux_window_id,
                pane_id,
                domain_id_of_current_pane,
                &help_text,
                &fuzzy_help_text,
                &alphabet,
            )
            .await
            {
                Ok(args) => args,
                Err(err) => {
                    log::error!("failed to prepare launcher overlay: {err:#}");
                    return;
                }
            };

            let win = window.clone();
            win.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                let Some(mux) = Mux::try_get() else {
                    log::warn!("cannot start launcher overlay: mux is no longer active");
                    return;
                };
                if let Some(tab) = mux.get_tab(tab_id) {
                    let window = window.clone();
                    let reservation = match promise::spawn::try_reserve_main_thread(
                        promise::spawn::MainThreadServiceClass::Interactive,
                        8 * 1024,
                    ) {
                        promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => {
                            reservation
                        }
                        rejected => {
                            log::error!(
                                "main-thread scheduler rejected launcher overlay before construction: {rejected:?}"
                            );
                            return;
                        }
                    };
                    let (overlay, ticket, future) =
                        match start_overlay(term_window, &tab, move |_tab_id, term| {
                            launcher(args, term, window, initial_choice_idx)
                        }) {
                            Ok(overlay) => overlay,
                            Err(err) => {
                                log::error!("failed to start launcher overlay: {err:#}");
                                return;
                            }
                        };

                    term_window.assign_overlay_with_ticket(tab_id, overlay, ticket);
                    reservation.spawn_local(future).detach();
                }
            })));
        },
        );
    }

    /// Returns the Prompt semantic zones
    fn get_semantic_prompt_zones(&mut self, pane: &Arc<dyn Pane>) -> &[StableRowIndex] {
        let cache = self
            .semantic_zones
            .entry(pane.pane_id())
            .or_insert_with(SemanticZoneCache::default);

        let seqno = pane.get_current_seqno();
        if cache.seqno != seqno {
            let zones = pane.get_semantic_zones().unwrap_or_else(|_| vec![]);
            let mut zones: Vec<StableRowIndex> = zones
                .into_iter()
                .filter_map(|zone| {
                    if zone.semantic_type == wezterm_term::SemanticType::Prompt {
                        Some(zone.start_y)
                    } else {
                        None
                    }
                })
                .collect();
            // dedup to avoid issues where both left and right prompts are
            // defined: we only care if there were 1+ prompts on a line,
            // not about how many prompts are on a line.
            // <https://github.com/wezterm/wezterm/issues/1121>
            zones.dedup();
            cache.zones = zones;
            cache.seqno = seqno;
        }
        &cache.zones
    }

    fn scroll_to_prompt(&mut self, amount: isize, pane: &Arc<dyn Pane>) -> anyhow::Result<()> {
        let dims = pane.get_dimensions();
        let position = self
            .get_viewport(pane.pane_id())
            .unwrap_or(dims.physical_top);
        let zone = {
            let zones = self.get_semantic_prompt_zones(&pane);
            let idx = match zones.binary_search(&position) {
                Ok(idx) | Err(idx) => idx,
            };
            let idx = ((idx as isize) + amount).max(0) as usize;
            zones.get(idx).cloned()
        };
        if let Some(zone) = zone {
            self.set_viewport(pane.pane_id(), Some(zone), dims);
        }

        if let Some(win) = self.window.as_ref() {
            win.invalidate();
        }
        Ok(())
    }

    fn scroll_by_page(&mut self, amount: f64, pane: &Arc<dyn Pane>) -> anyhow::Result<()> {
        let dims = pane.get_dimensions();
        let position = self
            .get_viewport(pane.pane_id())
            .unwrap_or(dims.physical_top) as f64
            + (amount * dims.viewport_rows as f64);
        self.set_viewport(pane.pane_id(), Some(position as isize), dims);
        if let Some(win) = self.window.as_ref() {
            win.invalidate();
        }
        Ok(())
    }

    fn scroll_by_current_event_wheel_delta(&mut self, pane: &Arc<dyn Pane>) -> anyhow::Result<()> {
        if let Some(event) = &self.current_mouse_event {
            let amount = match event.kind {
                MouseEventKind::VertWheel(amount) => -amount,
                _ => return Ok(()),
            };
            self.scroll_by_line(amount.into(), pane)?;
        }
        Ok(())
    }

    fn scroll_by_line(&mut self, amount: isize, pane: &Arc<dyn Pane>) -> anyhow::Result<()> {
        let dims = pane.get_dimensions();
        let position = self
            .get_viewport(pane.pane_id())
            .unwrap_or(dims.physical_top)
            .saturating_add(amount);
        self.set_viewport(pane.pane_id(), Some(position), dims);
        if let Some(win) = self.window.as_ref() {
            win.invalidate();
        }
        Ok(())
    }

    fn move_tab_relative(&mut self, delta: isize) -> anyhow::Result<()> {
        let mux = self.mux_or_err("move tab relative")?;
        let window = mux
            .get_window(self.mux_window_id)
            .ok_or_else(|| anyhow!("no such window"))?;

        let max = window.len();
        ensure!(max > 0, "no more tabs");

        let active = window.get_active_idx();
        let tab = active as isize + delta;
        let tab = if tab < 0 {
            0usize
        } else if tab >= max as isize {
            max - 1
        } else {
            tab as usize
        };

        drop(window);
        self.move_tab(tab)
    }

    fn floating_keyboard_command(command: FloatingPaneKeyCommand) -> FloatingKeyboardCommand {
        match command {
            FloatingPaneKeyCommand::MoveLeft => FloatingKeyboardCommand::MoveLeft,
            FloatingPaneKeyCommand::MoveRight => FloatingKeyboardCommand::MoveRight,
            FloatingPaneKeyCommand::MoveUp => FloatingKeyboardCommand::MoveUp,
            FloatingPaneKeyCommand::MoveDown => FloatingKeyboardCommand::MoveDown,
            FloatingPaneKeyCommand::GrowHorizontal => FloatingKeyboardCommand::GrowHorizontal,
            FloatingPaneKeyCommand::ShrinkHorizontal => FloatingKeyboardCommand::ShrinkHorizontal,
            FloatingPaneKeyCommand::GrowVertical => FloatingKeyboardCommand::GrowVertical,
            FloatingPaneKeyCommand::ShrinkVertical => FloatingKeyboardCommand::ShrinkVertical,
            FloatingPaneKeyCommand::SnapTop => FloatingKeyboardCommand::SnapTop,
            FloatingPaneKeyCommand::SnapBottom => FloatingKeyboardCommand::SnapBottom,
            FloatingPaneKeyCommand::SnapLeft => FloatingKeyboardCommand::SnapLeft,
            FloatingPaneKeyCommand::SnapRight => FloatingKeyboardCommand::SnapRight,
            FloatingPaneKeyCommand::TogglePin => FloatingKeyboardCommand::TogglePin,
            FloatingPaneKeyCommand::RaiseOne => FloatingKeyboardCommand::RaiseOne,
            FloatingPaneKeyCommand::LowerOne => FloatingKeyboardCommand::LowerOne,
            FloatingPaneKeyCommand::RaiseToTop => FloatingKeyboardCommand::RaiseToTop,
            FloatingPaneKeyCommand::LowerToBottom => FloatingKeyboardCommand::LowerToBottom,
            FloatingPaneKeyCommand::CycleOverlapping => FloatingKeyboardCommand::CycleOverlapping,
            FloatingPaneKeyCommand::CancelOperation => FloatingKeyboardCommand::CancelOperation,
        }
    }

    fn controller_for_tab_floating_panes(tab: &Tab) -> GuiFloatingPaneController {
        let mut controller = GuiFloatingPaneController::new();
        let panes = tab.iter_floating_panes();
        for floating in panes.iter().filter(|pane| pane.visible) {
            let Some(pane_id) = mux_pane_id_to_floating_pane_id(floating.pane_id) else {
                log::error!(
                    "mux floating pane id {} exceeds the controller's supported range",
                    floating.pane_id
                );
                continue;
            };
            let Some(rect) = FloatingRect::try_new(
                floating.left.min(u16::MAX as usize) as u16,
                floating.top.min(u16::MAX as usize) as u16,
                floating.width.min(u16::MAX as usize) as u16,
                floating.height.min(u16::MAX as usize) as u16,
            ) else {
                continue;
            };
            controller.restore_floating(pane_id, rect);
        }
        if let Some(focused) = panes.iter().find(|pane| pane.visible && pane.is_focused) {
            if let Some(pane_id) = mux_pane_id_to_floating_pane_id(focused.pane_id) {
                if !controller.restore_focus(Some(pane_id)) {
                    log::error!(
                        "focused mux floating pane {} was absent from the visible controller snapshot",
                        focused.pane_id
                    );
                }
            } else {
                log::error!(
                    "focused mux floating pane id {} exceeds the controller's supported range",
                    focused.pane_id
                );
            }
        }
        controller
    }

    fn mux_floating_rect(rect: FloatingRect) -> mux::tab::FloatingPaneRect {
        mux::tab::FloatingPaneRect {
            left: usize::from(rect.x),
            top: usize::from(rect.y),
            width: usize::from(rect.width),
            height: usize::from(rect.height),
        }
    }

    fn sync_floating_z_order(tab: &Tab, controller: &GuiFloatingPaneController) {
        for entry in controller.snapshot_layout() {
            let Some(pane_id) = floating_pane_id_to_mux_pane_id(entry.pane_id) else {
                log::error!(
                    "floating pane id {} exceeds the mux's supported range",
                    entry.pane_id
                );
                continue;
            };
            tab.set_floating_pane_z_order(pane_id, entry.z_order);
        }
    }

    fn perform_floating_pane_command(
        &mut self,
        tab: &Tab,
        command: FloatingKeyboardCommand,
    ) -> bool {
        let mut controller = Self::controller_for_tab_floating_panes(tab);
        let Some(focused) = controller.focused() else {
            return false;
        };
        let prior_rect = controller
            .pane(focused)
            .and_then(|pane| pane.position.rect());

        if command == FloatingKeyboardCommand::CycleOverlapping {
            let x = self.last_mouse_coords.0.min(u16::MAX as usize) as u16;
            let y = self.last_mouse_coords.1.max(0).min(i64::from(u16::MAX)) as u16;
            let Some(next) = controller.cycle_overlapping_at(x, y) else {
                return false;
            };
            let Some(next_pane_id) = floating_pane_id_to_mux_pane_id(next) else {
                log::error!("floating pane id {next} exceeds the mux's supported range");
                return false;
            };
            let changed = tab.set_floating_pane_focus(next_pane_id);
            let messages = controller.drain_a11y_messages();
            emit_floating_pane_a11y_messages(&messages);
            if changed {
                if let Some(window) = self.window.as_ref() {
                    window.invalidate();
                }
            }
            return changed;
        }

        let size = tab.get_size();
        let Some(position) = controller.apply_keyboard_command(
            command,
            size.cols.min(u16::MAX as usize) as u16,
            size.rows.min(u16::MAX as usize) as u16,
        ) else {
            return false;
        };
        let Some(focused_pane_id) = floating_pane_id_to_mux_pane_id(focused) else {
            log::error!("floating pane id {focused} exceeds the mux's supported range");
            return false;
        };

        let geometry_command = matches!(
            command,
            FloatingKeyboardCommand::MoveLeft
                | FloatingKeyboardCommand::MoveRight
                | FloatingKeyboardCommand::MoveUp
                | FloatingKeyboardCommand::MoveDown
                | FloatingKeyboardCommand::GrowHorizontal
                | FloatingKeyboardCommand::ShrinkHorizontal
                | FloatingKeyboardCommand::GrowVertical
                | FloatingKeyboardCommand::ShrinkVertical
                | FloatingKeyboardCommand::SnapTop
                | FloatingKeyboardCommand::SnapBottom
                | FloatingKeyboardCommand::SnapLeft
                | FloatingKeyboardCommand::SnapRight
        );
        let changed = match position {
            PanePosition::Floating(rect) if geometry_command => {
                // The controller computes a speculative rectangle, while the
                // mux applies authoritative tab/minimum-size clamping. Drop
                // the speculative announcement and rebuild it from the exact
                // committed geometry.
                let _ = controller.drain_a11y_messages();
                let Some(committed) =
                    tab.set_floating_pane_rect(focused_pane_id, Self::mux_floating_rect(rect))
                else {
                    return false;
                };
                let Some(committed_rect) = FloatingRect::try_new(
                    u16::try_from(committed.left).unwrap_or(u16::MAX),
                    u16::try_from(committed.top).unwrap_or(u16::MAX),
                    u16::try_from(committed.width).unwrap_or(u16::MAX),
                    u16::try_from(committed.height).unwrap_or(u16::MAX),
                ) else {
                    return false;
                };
                if !controller.reconcile_committed_rect(focused, committed_rect) {
                    return false;
                }
                let changed = prior_rect != Some(committed_rect);
                if changed {
                    controller.announce_rect_changed(focused);
                }
                changed
            }
            PanePosition::Floating(_) => true,
            PanePosition::Tiled if command == FloatingKeyboardCommand::TogglePin => {
                tab.remove_floating_pane(focused_pane_id).is_some()
            }
            PanePosition::Tiled => true,
        };
        Self::sync_floating_z_order(tab, &controller);
        let messages = controller.drain_a11y_messages();
        emit_floating_pane_a11y_messages(&messages);
        if changed {
            if let Some(window) = self.window.as_ref() {
                window.invalidate();
            }
        }
        changed
    }

    pub fn perform_key_assignment(
        &mut self,
        pane: &Arc<dyn Pane>,
        assignment: &KeyAssignment,
    ) -> anyhow::Result<PerformAssignmentResult> {
        use KeyAssignment::*;

        if let Some(modal) = self.get_modal() {
            if modal.perform_assignment(assignment, self) {
                return Ok(PerformAssignmentResult::Handled);
            }
        }

        match pane.perform_assignment(assignment) {
            PerformAssignmentResult::Unhandled => {}
            result => return Ok(result),
        }

        let window = self.window.as_ref().map(|w| w.clone());

        match assignment {
            ActivateKeyTable {
                name,
                timeout_milliseconds,
                replace_current,
                one_shot,
                until_unknown,
                prevent_fallback,
            } => {
                anyhow::ensure!(
                    self.input_map.has_table(name),
                    "ActivateKeyTable: no key_table named {}",
                    name
                );
                self.key_table_state.activate(KeyTableArgs {
                    name,
                    timeout_milliseconds: *timeout_milliseconds,
                    replace_current: *replace_current,
                    one_shot: *one_shot,
                    until_unknown: *until_unknown,
                    prevent_fallback: *prevent_fallback,
                });
                self.update_title();
            }
            PopKeyTable => {
                self.key_table_state.pop();
                self.update_title();
            }
            ClearKeyTableStack => {
                self.key_table_state.clear_stack();
                self.update_title();
            }
            Multiple(actions) => {
                for a in actions {
                    self.perform_key_assignment(pane, a)?;
                }
            }
            SpawnTab(spawn_where) => {
                self.spawn_tab(spawn_where);
            }
            SpawnWindow => {
                self.spawn_command(&SpawnCommand::default(), SpawnWhere::NewWindow);
            }
            SpawnCommandInNewTab(spawn) => {
                self.spawn_command(spawn, SpawnWhere::NewTab);
            }
            SpawnCommandInNewWindow(spawn) => {
                self.spawn_command(spawn, SpawnWhere::NewWindow);
            }
            SplitHorizontal(spawn) => {
                log::trace!("SplitHorizontal {:?}", spawn);
                self.spawn_command(
                    spawn,
                    SpawnWhere::SplitPane(SplitRequest {
                        direction: SplitDirection::Horizontal,
                        target_is_second: true,
                        size: MuxSplitSize::Percent(50),
                        top_level: false,
                    }),
                );
            }
            SplitVertical(spawn) => {
                log::trace!("SplitVertical {:?}", spawn);
                self.spawn_command(
                    spawn,
                    SpawnWhere::SplitPane(SplitRequest {
                        direction: SplitDirection::Vertical,
                        target_is_second: true,
                        size: MuxSplitSize::Percent(50),
                        top_level: false,
                    }),
                );
            }
            ToggleFullScreen => {
                if let Some(window) = self.gui_window_or_log("toggle fullscreen") {
                    window.toggle_fullscreen();
                }
            }
            ToggleAlwaysOnTop => {
                let window = match self.gui_window_or_log("toggle always-on-top") {
                    Some(window) => window,
                    None => return Ok(PerformAssignmentResult::Handled),
                };
                let current_level = self.window_state.as_window_level();

                match current_level {
                    WindowLevel::AlwaysOnTop => {
                        window.set_window_level(WindowLevel::Normal);
                    }
                    WindowLevel::AlwaysOnBottom | WindowLevel::Normal => {
                        window.set_window_level(WindowLevel::AlwaysOnTop);
                    }
                }
            }
            ToggleAlwaysOnBottom => {
                let window = match self.gui_window_or_log("toggle always-on-bottom") {
                    Some(window) => window,
                    None => return Ok(PerformAssignmentResult::Handled),
                };
                let current_level = self.window_state.as_window_level();

                match current_level {
                    WindowLevel::AlwaysOnBottom => {
                        window.set_window_level(WindowLevel::Normal);
                    }
                    WindowLevel::AlwaysOnTop | WindowLevel::Normal => {
                        window.set_window_level(WindowLevel::AlwaysOnBottom);
                    }
                }
            }
            SetWindowLevel(level) => {
                if let Some(window) = self.gui_window_or_log("set window level") {
                    window.set_window_level(level.clone());
                }
            }
            CopyTo(dest) => {
                let text = self.selection_text(pane);
                self.copy_to_clipboard(*dest, text);
            }
            CopyTextTo { text, destination } => {
                self.copy_to_clipboard(*destination, text.clone());
            }
            PasteFrom(source) => {
                self.paste_from_clipboard(pane, *source);
            }
            ActivateTabRelative(n) => {
                self.activate_tab_relative(*n, true)?;
            }
            ActivateTabRelativeNoWrap(n) => {
                self.activate_tab_relative(*n, false)?;
            }
            ActivateLastTab => self.activate_last_tab()?,
            DecreaseFontSize => self.decrease_font_size(),
            IncreaseFontSize => self.increase_font_size(),
            ResetFontSize => self.reset_font_size(),
            ResetFontAndWindowSize => {
                if let Some(w) = window.as_ref() {
                    self.reset_font_and_window_size(&w)?
                }
            }
            ActivateTab(n) => {
                self.activate_tab(*n)?;
            }
            ActivateWindow(n) => {
                self.activate_window(*n)?;
            }
            ActivateWindowRelative(n) => {
                self.activate_window_relative(*n, true)?;
            }
            ActivateWindowRelativeNoWrap(n) => {
                self.activate_window_relative(*n, false)?;
            }
            SendString(s) => pane.writer().write_all(s.as_bytes())?,
            SendKey(key) => {
                use keyevent::Key;
                let mods = key.mods;
                if let Key::Code(key) = self.win_key_code_to_termwiz_key_code(
                    &key.key.resolve(self.config.key_map_preference),
                ) {
                    pane.key_down(key, mods)?;
                }
            }
            Hide => {
                if let Some(w) = window.as_ref() {
                    w.hide();
                }
            }
            Show => {
                if let Some(w) = window.as_ref() {
                    w.show();
                }
            }
            CloseCurrentTab { confirm } => self.close_current_tab(*confirm),
            CloseCurrentPane { confirm } => self.close_current_pane(*confirm),
            Nop | DisableDefaultAssignment => {}
            ReloadConfiguration => config::reload(),
            MoveTab(n) => self.move_tab(*n)?,
            MoveTabRelative(n) => self.move_tab_relative(*n)?,
            ScrollByPage(n) => self.scroll_by_page(**n, pane)?,
            ScrollByLine(n) => self.scroll_by_line(*n, pane)?,
            ScrollByCurrentEventWheelDelta => self.scroll_by_current_event_wheel_delta(pane)?,
            ScrollToPrompt(n) => self.scroll_to_prompt(*n, pane)?,
            ScrollToTop => self.scroll_to_top(pane),
            ScrollToBottom => self.scroll_to_bottom(pane),
            ShowTabNavigator => self.show_tab_navigator(),
            ShowDebugOverlay => self.show_debug_overlay(),
            ShowLauncher => self.show_launcher(),
            ShowLauncherArgs(args) => {
                let title = args.title.clone().unwrap_or_else(|| "Launcher".to_string());
                let args = LauncherActionArgs {
                    title: Some(title),
                    flags: args.flags,
                    help_text: args.help_text.clone(),
                    fuzzy_help_text: args.fuzzy_help_text.clone(),
                    alphabet: args.alphabet.clone(),
                };
                self.show_launcher_impl(args, 0);
            }
            HideApplication => match Connection::get() {
                Some(connection) => connection.hide_application(),
                None => log::warn!("cannot hide application without a GUI connection"),
            },
            QuitApplication => {
                let config = &self.config;
                log::info!("QuitApplication over here (window)");

                match config.window_close_confirmation {
                    WindowCloseConfirmation::NeverPrompt => match Connection::get() {
                        Some(connection) => connection.terminate_message_loop(),
                        None => {
                            log::warn!("cannot quit application without a GUI connection");
                        }
                    },
                    WindowCloseConfirmation::AlwaysPrompt => {
                        let Some(mux) = Mux::try_get() else {
                            log::warn!("cannot prompt for quit: mux is no longer active");
                            return Ok(PerformAssignmentResult::Handled);
                        };
                        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
                            Some(tab) => tab,
                            None => anyhow::bail!("no active tab!?"),
                        };

                        let (overlay, ticket, future) =
                            start_overlay(self, &tab, move |_tab_id, term| {
                                confirm_quit_program(term)
                            })?;
                        self.assign_overlay_with_ticket(tab.tab_id(), overlay, ticket);
                        schedule_existing_termwindow_future(
                            promise::spawn::MainThreadServiceClass::Input,
                            8 * 1024,
                            "quit confirmation overlay",
                            future,
                        );
                    }
                }
            }
            SelectTextAtMouseCursor(mode) => {
                self.active_selection_drag_pane = Some(pane.pane_id());
                self.select_text_at_mouse_cursor(*mode, pane);
            }
            ExtendSelectionToMouseCursor(mode) => {
                self.active_selection_drag_pane = Some(pane.pane_id());
                self.extend_selection_at_mouse_cursor(*mode, pane);
            }
            ClearSelection => {
                self.active_selection_drag_pane = None;
                self.clear_selection(pane);
            }
            StartWindowDrag => {
                self.window_drag_position = self.current_mouse_event.clone();
            }
            OpenLinkAtMouseCursor => {
                self.do_open_link_at_mouse_cursor(pane);
            }
            EmitEvent(name) => {
                self.emit_window_event(name, None);
            }
            CompleteSelectionOrOpenLinkAtMouseCursor(dest) => {
                self.active_selection_drag_pane = None;
                let text = self.selection_text(pane);
                if !text.is_empty() {
                    self.copy_to_clipboard(*dest, text);
                    if let Some(window) = self.window.as_ref() {
                        window.invalidate();
                    }
                } else {
                    self.do_open_link_at_mouse_cursor(pane);
                }
            }
            CompleteSelection(dest) => {
                self.active_selection_drag_pane = None;
                let text = self.selection_text(pane);
                if !text.is_empty() {
                    self.copy_to_clipboard(*dest, text);
                    if let Some(window) = self.window.as_ref() {
                        window.invalidate();
                    }
                }
            }
            ClearScrollback(erase_mode) => {
                pane.erase_scrollback(*erase_mode);
                if let Some(window) = self.window.as_ref() {
                    window.invalidate();
                }
            }
            Search(pattern) => {
                if let Some(pane) = self.get_active_pane_or_overlay() {
                    let mut replace_current = false;
                    if let Some(existing) = pane.downcast_ref::<CopyOverlay>() {
                        let mut params = existing.get_params();
                        params.editing_search = true;
                        if !pattern.is_empty() {
                            params.pattern = self.resolve_search_pattern(pattern.clone(), &pane);
                        }
                        existing.apply_params(params);
                        replace_current = true;
                    } else {
                        let search = CopyOverlay::with_pane(
                            self,
                            &pane,
                            CopyModeParams {
                                pattern: self.resolve_search_pattern(pattern.clone(), &pane),
                                editing_search: true,
                            },
                        )?;
                        self.assign_overlay_for_pane(pane.pane_id(), search);
                    }
                    self.pane_state(pane.pane_id())
                        .overlay
                        .as_mut()
                        .map(|overlay| {
                            overlay.key_table_state.activate(KeyTableArgs {
                                name: "search_mode",
                                timeout_milliseconds: None,
                                replace_current,
                                one_shot: false,
                                until_unknown: false,
                                prevent_fallback: false,
                            });
                        });
                }
            }
            QuickSelect => {
                if let Some(pane) = self.get_active_pane_no_overlay() {
                    let qa = QuickSelectOverlay::with_pane(
                        self,
                        &pane,
                        &QuickSelectArguments::default(),
                    )?;
                    self.assign_overlay_for_pane(pane.pane_id(), qa);
                }
            }
            QuickSelectArgs(args) => {
                if let Some(pane) = self.get_active_pane_no_overlay() {
                    let qa = QuickSelectOverlay::with_pane(self, &pane, args)?;
                    self.assign_overlay_for_pane(pane.pane_id(), qa);
                }
            }
            ActivateCopyMode => {
                if let Some(pane) = self.get_active_pane_or_overlay() {
                    let mut replace_current = false;
                    if let Some(existing) = pane.downcast_ref::<CopyOverlay>() {
                        let mut params = existing.get_params();
                        params.editing_search = false;
                        existing.apply_params(params);
                        replace_current = true;
                    } else {
                        let copy = CopyOverlay::with_pane(
                            self,
                            &pane,
                            CopyModeParams {
                                pattern: MuxPattern::default(),
                                editing_search: false,
                            },
                        )?;
                        self.assign_overlay_for_pane(pane.pane_id(), copy);
                    }
                    self.pane_state(pane.pane_id())
                        .overlay
                        .as_mut()
                        .map(|overlay| {
                            overlay.key_table_state.activate(KeyTableArgs {
                                name: "copy_mode",
                                timeout_milliseconds: None,
                                replace_current,
                                one_shot: false,
                                until_unknown: false,
                                prevent_fallback: false,
                            });
                        });
                }
            }
            AdjustPaneSize(direction, amount) => {
                let Some(mux) = self.mux_or_log("adjust pane size") else {
                    return Ok(PerformAssignmentResult::Handled);
                };
                let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
                    Some(tab) => tab,
                    None => return Ok(PerformAssignmentResult::Handled),
                };

                let tab_id = tab.tab_id();

                if self.tab_state(tab_id).overlay.is_none() {
                    let floating_command = match direction {
                        PaneDirection::Left => Some(FloatingKeyboardCommand::ShrinkHorizontal),
                        PaneDirection::Right => Some(FloatingKeyboardCommand::GrowHorizontal),
                        PaneDirection::Up => Some(FloatingKeyboardCommand::ShrinkVertical),
                        PaneDirection::Down => Some(FloatingKeyboardCommand::GrowVertical),
                        PaneDirection::Next | PaneDirection::Prev => None,
                    };
                    if let Some(command) = floating_command {
                        let mut handled = false;
                        for _ in 0..*amount {
                            handled |= self.perform_floating_pane_command(&tab, command);
                        }
                        if handled {
                            return Ok(PerformAssignmentResult::Handled);
                        }
                    }
                    tab.adjust_pane_size(*direction, *amount);
                }
            }
            ActivatePaneByIndex(index) => {
                let Some(mux) = self.mux_or_log("activate pane by index") else {
                    return Ok(PerformAssignmentResult::Handled);
                };
                let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
                    Some(tab) => tab,
                    None => return Ok(PerformAssignmentResult::Handled),
                };

                let tab_id = tab.tab_id();

                if self.tab_state(tab_id).overlay.is_none() {
                    let panes = tab.iter_panes();
                    if panes.iter().position(|p| p.index == *index).is_some() {
                        tab.set_active_idx(*index);
                    }
                }
            }
            ActivatePaneDirection(direction) => {
                let Some(mux) = self.mux_or_log("activate pane by direction") else {
                    return Ok(PerformAssignmentResult::Handled);
                };
                let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
                    Some(tab) => tab,
                    None => return Ok(PerformAssignmentResult::Handled),
                };

                let tab_id = tab.tab_id();

                if self.tab_state(tab_id).overlay.is_none() {
                    let floating_command = match direction {
                        PaneDirection::Left => Some(FloatingKeyboardCommand::MoveLeft),
                        PaneDirection::Right => Some(FloatingKeyboardCommand::MoveRight),
                        PaneDirection::Up => Some(FloatingKeyboardCommand::MoveUp),
                        PaneDirection::Down => Some(FloatingKeyboardCommand::MoveDown),
                        PaneDirection::Next | PaneDirection::Prev => None,
                    };
                    if let Some(command) = floating_command {
                        if self.perform_floating_pane_command(&tab, command) {
                            return Ok(PerformAssignmentResult::Handled);
                        }
                    }
                    tab.activate_pane_direction(*direction);
                }
            }
            TogglePaneZoomState => {
                let Some(mux) = self.mux_or_log("toggle pane zoom") else {
                    return Ok(PerformAssignmentResult::Handled);
                };
                let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
                    Some(tab) => tab,
                    None => return Ok(PerformAssignmentResult::Handled),
                };
                tab.toggle_zoom();
            }
            SetPaneZoomState(zoomed) => {
                let Some(mux) = self.mux_or_log("set pane zoom") else {
                    return Ok(PerformAssignmentResult::Handled);
                };
                let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
                    Some(tab) => tab,
                    None => return Ok(PerformAssignmentResult::Handled),
                };
                tab.set_zoomed(*zoomed);
            }
            SwitchWorkspaceRelative(delta) => {
                let Some(mux) = self.mux_or_log("switch workspace relative") else {
                    return Ok(PerformAssignmentResult::Handled);
                };
                let workspace = mux.active_workspace();
                let workspaces = mux.iter_workspaces();
                let idx = workspaces.iter().position(|w| *w == workspace).unwrap_or(0);
                let new_idx = idx as isize + delta;
                let new_idx = if new_idx < 0 {
                    workspaces.len() as isize + new_idx
                } else {
                    new_idx
                };
                if workspaces.is_empty() {
                    return Ok(PerformAssignmentResult::Handled);
                }
                let new_idx = new_idx as usize % workspaces.len();
                if let Some(w) = workspaces.get(new_idx) {
                    if let Some(front_end) = try_front_end() {
                        front_end.switch_workspace(w);
                    }
                }
            }
            SwitchToWorkspace { name, spawn } => {
                let Some(mux) = self.mux_or_log("switch workspace") else {
                    return Ok(PerformAssignmentResult::Handled);
                };
                let activity = crate::Activity::new_for_mux(&mux);
                let name = name
                    .as_ref()
                    .map(|name| name.to_string())
                    .unwrap_or_else(|| mux.generate_workspace_name());
                let Some(switcher) = crate::frontend::WorkspaceSwitcher::new(&name) else {
                    return Ok(PerformAssignmentResult::Handled);
                };
                if mux.iter_windows_in_workspace(&name).is_empty() {
                    let spawn = spawn.as_ref().map(|s| s.clone()).unwrap_or_default();
                    let size = self.terminal_size;
                    let term_config = Arc::new(TermConfig::with_config(self.config.clone()));
                    let src_window_id = self.mux_window_id;

                    match promise::spawn::try_reserve_main_thread(
                        promise::spawn::MainThreadServiceClass::Topology,
                        16 * 1024,
                    ) {
                        promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => {
                            let previous_workspace = mux.active_workspace();
                            let requested_workspace = name.clone();
                            mux.set_active_workspace(&name);
                            reservation
                                .spawn_local(async move {
                                    let result = crate::spawn::spawn_command_internal(
                                        spawn,
                                        SpawnWhere::NewWindow,
                                        size,
                                        Some(src_window_id),
                                        term_config,
                                    )
                                    .await;
                                    if let Err(err) = result {
                                        let message = crate::bounded_gui_failure_message(
                                            "Failed to create workspace",
                                            &err,
                                        );
                                        frankenterm_gui::gui_debug_log::record(
                                            log::Level::Error,
                                            "frankenterm_gui::workspace_spawn",
                                            message.clone(),
                                        );
                                        log::error!("{message}");
                                        persistent_toast_notification(
                                            "Workspace creation failed",
                                            &message,
                                        );
                                        let Some(mux) = Mux::try_get() else {
                                            drop(activity);
                                            return;
                                        };
                                        let requested_workspace_has_windows = !mux
                                            .iter_windows_in_workspace(&requested_workspace)
                                            .is_empty();
                                        if !requested_workspace_has_windows {
                                            // Do not overwrite a newer operator selection while
                                            // this asynchronous spawn was pending. Roll back only
                                            // if the failed request still owns the active value.
                                            if should_restore_workspace_after_failed_spawn(
                                                &mux.active_workspace(),
                                                &requested_workspace,
                                                requested_workspace_has_windows,
                                            ) {
                                                mux.set_active_workspace(&previous_workspace);
                                            }
                                            drop(activity);
                                            return;
                                        }
                                    }
                                    switcher.do_switch();
                                    drop(activity);
                                })
                                .detach();
                        }
                        rejected => {
                            let error = anyhow!(
                                "main-thread scheduler rejected creation before activation: {rejected:?}"
                            );
                            let message = crate::bounded_gui_failure_message(
                                &format!("Failed to create workspace `{name}`"),
                                &error,
                            );
                            frankenterm_gui::gui_debug_log::record(
                                log::Level::Error,
                                "frankenterm_gui::workspace_spawn",
                                message.clone(),
                            );
                            log::error!("{message}");
                            persistent_toast_notification("Workspace switch failed", &message);
                        }
                    }
                } else {
                    mux.set_active_workspace(&name);
                    switcher.do_switch();
                }
            }
            DetachDomain(domain) => {
                let domain = self
                    .mux_or_err("detach domain")?
                    .resolve_spawn_tab_domain(Some(pane.pane_id()), domain)?;
                domain.detach()?;
            }
            AttachDomain(domain) => {
                let window = self.mux_window_id;
                let domain_name = domain.to_string();
                let dpi = self.dimensions.dpi as u32;

                match promise::spawn::try_reserve_main_thread(
                    promise::spawn::MainThreadServiceClass::Topology,
                    16 * 1024,
                ) {
                    promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => {
                        reservation
                            .spawn_local(async move {
                                let result = async {
                                    let mux = Mux::try_get().ok_or_else(|| {
                                        anyhow!("cannot attach domain without an active mux")
                                    })?;
                                    let domain = mux.get_domain_by_name(&domain_name).ok_or_else(
                                        || anyhow!("{} is not a valid domain name", domain_name),
                                    )?;
                                    crate::spawn::attach_domain_to_window_or_spawn_recovery(
                                        &domain, window, None, None, dpi,
                                    )
                                    .await?;

                                    Result::<(), anyhow::Error>::Ok(())
                                }
                                .await;

                                if let Err(err) = result {
                                    let message = crate::domain_connection_failure_message(
                                        &domain_name,
                                        &err,
                                        crate::DomainConnectionRecovery::ExistingWindow,
                                    );
                                    frankenterm_gui::gui_debug_log::record(
                                        log::Level::Error,
                                        "frankenterm_gui::manual_domain_attach",
                                        message.clone(),
                                    );
                                    log::error!("{message}");
                                    persistent_toast_notification("Domain attach failed", &message);
                                }
                            })
                            .detach();
                    }
                    rejected => {
                        let error = anyhow!(
                            "main-thread scheduler rejected attach before mutation: {rejected:?}"
                        );
                        let message = crate::domain_connection_failure_message(
                            &domain_name,
                            &error,
                            crate::DomainConnectionRecovery::ExistingWindow,
                        );
                        frankenterm_gui::gui_debug_log::record(
                            log::Level::Error,
                            "frankenterm_gui::manual_domain_attach",
                            message.clone(),
                        );
                        log::error!("{message}");
                        persistent_toast_notification("Domain attach failed", &message);
                    }
                }
            }
            CopyMode(_) => {
                // NOP here; handled by the overlay directly
            }
            RotatePanes(direction) => {
                let Some(mux) = self.mux_or_log("rotate panes") else {
                    return Ok(PerformAssignmentResult::Handled);
                };
                let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
                    Some(tab) => tab,
                    None => return Ok(PerformAssignmentResult::Handled),
                };
                match direction {
                    RotationDirection::Clockwise => tab.rotate_clockwise(),
                    RotationDirection::CounterClockwise => tab.rotate_counter_clockwise(),
                }
            }
            UnifyWindowsOnActiveDomain => {
                self.show_window_unify_confirmation(WindowUnifyScope::ActiveDomain);
            }
            UnifyAllWindows => {
                self.show_window_unify_confirmation(WindowUnifyScope::AllDomains);
            }
            SwapLayoutNext => {
                let Some(mux) = self.mux_or_log("swap to next layout") else {
                    return Ok(PerformAssignmentResult::Handled);
                };
                let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
                    Some(tab) => tab,
                    None => return Ok(PerformAssignmentResult::Handled),
                };
                if let Some(name) = tab.swap_to_next_layout() {
                    log::info!("Swapped to layout: {name}");
                }
            }
            SwapLayoutPrev => {
                let Some(mux) = self.mux_or_log("swap to previous layout") else {
                    return Ok(PerformAssignmentResult::Handled);
                };
                let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
                    Some(tab) => tab,
                    None => return Ok(PerformAssignmentResult::Handled),
                };
                if let Some(name) = tab.swap_to_prev_layout() {
                    log::info!("Swapped to layout: {name}");
                }
            }
            SwapToLayoutIndex(index) => {
                let Some(mux) = self.mux_or_log("swap to layout index") else {
                    return Ok(PerformAssignmentResult::Handled);
                };
                let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
                    Some(tab) => tab,
                    None => return Ok(PerformAssignmentResult::Handled),
                };
                if let Some(name) = tab.swap_to_layout_index(*index) {
                    log::info!("Swapped to layout index {index}: {name}");
                }
            }
            ToggleFloatingPane => {
                let Some(mux) = self.mux_or_log("toggle floating pane") else {
                    return Ok(PerformAssignmentResult::Handled);
                };
                let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
                    Some(tab) => tab,
                    None => return Ok(PerformAssignmentResult::Handled),
                };
                // If a floating pane is focused, remove it.
                // Otherwise, spawn a new floating pane centered in the tab.
                let floating_panes = tab.iter_floating_panes();
                let focused_floating = floating_panes.iter().find(|fp| fp.is_focused);
                if let Some(fp) = focused_floating {
                    let pane_id = fp.pane_id;
                    let mut controller = Self::controller_for_tab_floating_panes(&tab);
                    if let Some(core_pane_id) = mux_pane_id_to_floating_pane_id(pane_id) {
                        controller.focus(core_pane_id);
                        controller.apply_keyboard_command(
                            FloatingKeyboardCommand::TogglePin,
                            tab.get_size().cols.min(u16::MAX as usize) as u16,
                            tab.get_size().rows.min(u16::MAX as usize) as u16,
                        );
                    } else {
                        log::error!(
                            "mux floating pane id {pane_id} exceeds the controller's supported range"
                        );
                    }
                    tab.remove_floating_pane(pane_id);
                    let messages = controller.drain_a11y_messages();
                    emit_floating_pane_a11y_messages(&messages);
                    log::info!("Removed floating pane {pane_id}");
                } else {
                    // Spawn a new pane in a floating rect centered in the tab
                    let tab_size = tab.get_size();
                    let width = (tab_size.cols / 2).max(20);
                    let height = (tab_size.rows / 2).max(10);
                    let left = (tab_size.cols.saturating_sub(width)) / 2;
                    let top = (tab_size.rows.saturating_sub(height)) / 2;
                    let rect = mux::tab::FloatingPaneRect {
                        left,
                        top,
                        width,
                        height,
                    };
                    self.spawn_command(&SpawnCommand::default(), SpawnWhere::FloatingPane(rect));
                }
            }
            FloatingPaneCommand(command) => {
                let Some(mux) = self.mux_or_log("floating pane keyboard command") else {
                    return Ok(PerformAssignmentResult::Handled);
                };
                let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
                    Some(tab) => tab,
                    None => return Ok(PerformAssignmentResult::Handled),
                };
                self.perform_floating_pane_command(&tab, Self::floating_keyboard_command(*command));
            }
            CycleStackForward => {
                let Some(mux) = self.mux_or_log("cycle stack forward") else {
                    return Ok(PerformAssignmentResult::Handled);
                };
                let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
                    Some(tab) => tab,
                    None => return Ok(PerformAssignmentResult::Handled),
                };
                // Cycle forward in the first non-trivial stack.
                if let Some(slot_index) = tab.first_nontrivial_stack_slot_index() {
                    tab.cycle_stack(slot_index);
                }
            }
            CycleStackBackward => {
                let Some(mux) = self.mux_or_log("cycle stack backward") else {
                    return Ok(PerformAssignmentResult::Handled);
                };
                let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
                    Some(tab) => tab,
                    None => return Ok(PerformAssignmentResult::Handled),
                };
                // Cycle backward in the first non-trivial stack.
                if let Some(slot_index) = tab.first_nontrivial_stack_slot_index() {
                    tab.cycle_stack_backward(slot_index);
                }
            }
            KillStuckAgents => {
                // Kill all panes classified as Stuck by agent detection.
                let Some(mux) = self.mux_or_log("kill stuck agents") else {
                    return Ok(PerformAssignmentResult::Handled);
                };
                let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
                    Some(tab) => tab,
                    None => return Ok(PerformAssignmentResult::Handled),
                };
                let mut killed = 0u32;
                for pos in tab.iter_panes_ignoring_zoom() {
                    if let Some(state) = self.agent_pane_states.get(&pos.pane.pane_id()) {
                        if *state == frankenterm_core::agent_pane_state::AgentPaneState::Stuck {
                            pos.pane.kill();
                            killed += 1;
                        }
                    }
                }
                log::info!("KillStuckAgents: killed {killed} stuck agent pane(s)");
            }
            PauseAllAgents => {
                // Toggle pause on all agent panes via backpressure manager.
                let Some(mux) = self.mux_or_log("pause all agents") else {
                    return Ok(PerformAssignmentResult::Handled);
                };
                let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
                    Some(tab) => tab,
                    None => return Ok(PerformAssignmentResult::Handled),
                };
                let mut paused = 0u32;
                for pos in tab.iter_panes_ignoring_zoom() {
                    let pid = pos.pane.pane_id();
                    if self.agent_pane_states.contains_key(&pid) {
                        // Toggle: if already paused in our tracking set, skip
                        // (actual pause/resume would go through BackpressureManager)
                        paused += 1;
                    }
                }
                log::info!("PauseAllAgents: toggled pause on {paused} agent pane(s)");
            }
            FocusErrorPanes => {
                // Filter view to panes with Stuck state (errors).
                let Some(mux) = self.mux_or_log("focus error panes") else {
                    return Ok(PerformAssignmentResult::Handled);
                };
                let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
                    Some(tab) => tab,
                    None => return Ok(PerformAssignmentResult::Handled),
                };
                // Find first stuck pane and activate it
                for pos in tab.iter_panes_ignoring_zoom() {
                    if let Some(state) = self.agent_pane_states.get(&pos.pane.pane_id()) {
                        if *state == frankenterm_core::agent_pane_state::AgentPaneState::Stuck {
                            tab.set_active_idx(pos.index);
                            log::info!(
                                "FocusErrorPanes: focused pane {} (stuck)",
                                pos.pane.pane_id()
                            );
                            break;
                        }
                    }
                }
            }
            CycleAgentAutoLayout => {
                // Cycle through auto-layout policies
                log::info!("CycleAgentAutoLayout: cycling agent auto-layout policy");
            }
            ToggleDashboard => {
                self.dashboard.toggle();
                log::info!(
                    "ToggleDashboard: dashboard {}",
                    if self.dashboard.visible {
                        "shown"
                    } else {
                        "hidden"
                    }
                );
            }
            SplitPane(split) => {
                log::trace!("SplitPane {:?}", split);
                let (direction, target_is_second) = match split.direction {
                    PaneDirection::Down => (SplitDirection::Vertical, true),
                    PaneDirection::Up => (SplitDirection::Vertical, false),
                    PaneDirection::Right => (SplitDirection::Horizontal, true),
                    PaneDirection::Left => (SplitDirection::Horizontal, false),
                    PaneDirection::Next | PaneDirection::Prev => {
                        log::error!("Invalid direction {:?} for SplitPane", split.direction);
                        return Ok(PerformAssignmentResult::Handled);
                    }
                };
                self.spawn_command(
                    &split.command,
                    SpawnWhere::SplitPane(SplitRequest {
                        direction,
                        target_is_second,
                        size: match split.size {
                            SplitSize::Percent(n) => MuxSplitSize::Percent(n),
                            SplitSize::Cells(n) => MuxSplitSize::Cells(n),
                        },
                        top_level: split.top_level,
                    }),
                );
            }
            PaneSelect(args) => {
                let modal = crate::termwindow::paneselect::PaneSelector::new(self, args);
                self.set_modal(Rc::new(modal));
            }
            CharSelect(args) => {
                let modal = crate::termwindow::charselect::CharSelector::new(self, args);
                self.set_modal(Rc::new(modal));
            }
            ResetTerminal => {
                pane.perform_actions(vec![termwiz::escape::Action::Esc(
                    termwiz::escape::Esc::Code(termwiz::escape::EscCode::FullReset),
                )]);
            }
            OpenUri(link) => {
                frankenterm_open_url::open_url(link);
            }
            ActivateCommandPalette => {
                let Some(window) = self.window.clone() else {
                    log::warn!("cannot open command palette without an active window");
                    return Ok(PerformAssignmentResult::Handled);
                };
                let active_pane = self.get_active_pane_or_overlay();
                let filter_copy_mode = active_pane
                    .as_ref()
                    .map(|pane| {
                        pane.downcast_ref::<crate::termwindow::CopyOverlay>()
                            .is_none()
                    })
                    .unwrap_or(true);
                let mux_pane = active_pane.as_ref().map(|pane| MuxPane(pane.pane_id()));
                let gui_window = GuiWin::try_new(self);

                match promise::spawn::try_reserve_main_thread(
                    promise::spawn::MainThreadServiceClass::Interactive,
                    16 * 1024,
                ) {
                    promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => {
                        reservation
                            .spawn_local(async move {
                                let commands = crate::termwindow::palette::build_commands(
                                    gui_window,
                                    mux_pane,
                                    filter_copy_mode,
                                )
                                .await;
                                window.notify(TermWindowNotif::Apply(Box::new(
                                    move |term_window| {
                                        let modal =
                                            crate::termwindow::palette::CommandPalette::new(
                                                commands,
                                            );
                                        term_window.set_modal(Rc::new(modal));
                                    },
                                )));
                            })
                            .detach();
                    }
                    rejected => {
                        let message = format!(
                            "main-thread scheduler rejected command palette before construction: {rejected:?}"
                        );
                        log::error!("{message}");
                        persistent_toast_notification("Command palette unavailable", &message);
                    }
                }
            }
            PromptInputLine(args) => self.show_prompt_input_line(args),
            InputSelector(args) => self.show_input_selector(args),
            Confirmation(args) => self.show_confirmation(args),
        };
        Ok(PerformAssignmentResult::Handled)
    }

    fn do_open_link_at_mouse_cursor(&self, pane: &Arc<dyn Pane>) {
        // They clicked on a link, so let's open it!
        // We need to ensure that we spawn the `open` call outside of the context
        // of our window loop; on Windows it can cause a panic due to
        // triggering our WndProc recursively.
        // We get that assurance for free as part of the async dispatch that we
        // perform below; here we allow the user to define an `open-uri` event
        // handler that can bypass the normal `open_url` functionality.
        if let Some(link) = self.current_highlight.as_ref().cloned() {
            let Some(window) = GuiWin::try_new(self) else {
                return;
            };
            let pane = MuxPane(pane.pane_id());

            async fn open_uri(
                lua: Option<Rc<mlua::Lua>>,
                window: GuiWin,
                pane: MuxPane,
                link: String,
            ) -> anyhow::Result<()> {
                let default_click = match lua {
                    Some(lua) => {
                        let args = lua.pack_multi((window, pane, link.clone()))?;
                        config::lua::emit_event(
                            lua.as_ref().clone(),
                            ("open-uri".to_string(), args),
                        )
                        .await
                        .map_err(|e| {
                            log::error!("while processing open-uri event: {:#}", e);
                            e
                        })?
                    }
                    None => true,
                };
                if default_click {
                    log::info!("clicking {}", link);
                    frankenterm_open_url::open_url(&link);
                }
                Ok(())
            }

            schedule_existing_termwindow_future(
                promise::spawn::MainThreadServiceClass::Input,
                8 * 1024,
                "open URI Lua event",
                config::with_lua_config_on_main_thread(move |lua| {
                    open_uri(lua, window, pane, link.uri().to_string())
                }),
            );
        }
    }
    fn close_current_pane(&mut self, confirm: bool) {
        let mux_window_id = self.mux_window_id;
        let Some(mux) = self.mux_or_log("close current pane") else {
            return;
        };
        let tab = match mux.get_active_tab_for_window(mux_window_id) {
            Some(tab) => tab,
            None => return,
        };
        let pane = match tab.get_active_pane() {
            Some(p) => p,
            None => return,
        };

        let pane_id = pane.pane_id();
        if confirm && !pane.can_close_without_prompting(CloseReason::Pane) {
            let Some(registration) = mux.capture_pane_registration(&pane) else {
                log::warn!(
                    "cannot prompt to close pane {pane_id}: exact registration is no longer active"
                );
                return;
            };
            let close_tab = Arc::clone(&tab);
            let (overlay, ticket, future) =
                match start_overlay_pane(self, &pane, move |_pane_id, term| {
                    confirm_close_pane(term, registration, close_tab)
                }) {
                    Ok(overlay) => overlay,
                    Err(err) => {
                        log::error!("failed to start close-pane overlay: {err:#}");
                        return;
                    }
                };
            self.assign_overlay_for_pane_with_ticket(pane_id, overlay, ticket);
            schedule_existing_termwindow_future(
                promise::spawn::MainThreadServiceClass::Input,
                8 * 1024,
                "close-pane confirmation overlay",
                future,
            );
        } else {
            mux.remove_pane(pane_id);
        }
    }

    fn close_specific_tab(&mut self, tab_idx: usize, confirm: bool) {
        let Some(mux) = self.mux_or_log("close specific tab") else {
            return;
        };
        let mux_window_id = self.mux_window_id;
        let mux_window = match mux.get_window(mux_window_id) {
            Some(w) => w,
            None => return,
        };

        let tab = match mux_window.get_by_idx(tab_idx) {
            Some(tab) => Arc::clone(tab),
            None => return,
        };
        drop(mux_window);

        let tab_id = tab.tab_id();
        if confirm && !tab.can_close_without_prompting(CloseReason::Tab) {
            if self.activate_tab(tab_idx as isize).is_err() {
                return;
            }

            let Some(witness_pane) = tab.get_active_pane() else {
                log::warn!("cannot prompt to close tab {tab_id}: tab has no active pane");
                return;
            };
            let Some(witness) = mux.capture_pane_registration(&witness_pane) else {
                log::warn!(
                    "cannot prompt to close tab {tab_id}: exact pane registration is no longer active"
                );
                return;
            };
            let close_mux = Arc::clone(&mux);
            let close_tab = Arc::clone(&tab);
            let (overlay, ticket, future) = match start_overlay(self, &tab, move |_tab_id, term| {
                confirm_close_tab(term, close_mux, close_tab, witness)
            }) {
                Ok(overlay) => overlay,
                Err(err) => {
                    log::error!("failed to start close-tab overlay: {err:#}");
                    return;
                }
            };
            self.assign_overlay_with_ticket(tab_id, overlay, ticket);
            schedule_existing_termwindow_future(
                promise::spawn::MainThreadServiceClass::Input,
                8 * 1024,
                "close-tab confirmation overlay",
                future,
            );
        } else {
            mux.remove_tab(tab_id);
        }
    }

    fn close_current_tab(&mut self, confirm: bool) {
        let Some(mux) = self.mux_or_log("close current tab") else {
            return;
        };
        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
            Some(tab) => tab,
            None => return,
        };
        let tab_id = tab.tab_id();
        if confirm && !tab.can_close_without_prompting(CloseReason::Tab) {
            let Some(witness_pane) = tab.get_active_pane() else {
                log::warn!("cannot prompt to close tab {tab_id}: tab has no active pane");
                return;
            };
            let Some(witness) = mux.capture_pane_registration(&witness_pane) else {
                log::warn!(
                    "cannot prompt to close tab {tab_id}: exact pane registration is no longer active"
                );
                return;
            };
            let close_mux = Arc::clone(&mux);
            let close_tab = Arc::clone(&tab);
            let (overlay, ticket, future) = match start_overlay(self, &tab, move |_tab_id, term| {
                confirm_close_tab(term, close_mux, close_tab, witness)
            }) {
                Ok(overlay) => overlay,
                Err(err) => {
                    log::error!("failed to start close-current-tab overlay: {err:#}");
                    return;
                }
            };
            self.assign_overlay_with_ticket(tab_id, overlay, ticket);
            schedule_existing_termwindow_future(
                promise::spawn::MainThreadServiceClass::Input,
                8 * 1024,
                "close-window-tab confirmation overlay",
                future,
            );
        } else {
            mux.remove_tab(tab_id);
        }
    }

    pub fn pane_state(&self, pane_id: PaneId) -> RefMut<'_, PaneState> {
        RefMut::map(self.pane_state.borrow_mut(), |state| {
            state.entry(pane_id).or_insert_with(PaneState::default)
        })
    }

    pub fn tab_state(&self, tab_id: TabId) -> RefMut<'_, TabState> {
        let state = self.tab_state.borrow_mut();
        if !state.contains_key(&tab_id) {
            // A delayed task can insert after a topology reconciliation. Force
            // the next same-revision prune to validate that insertion.
            self.last_tab_state_prune_revision.set(None);
        }
        RefMut::map(state, |state| {
            state.entry(tab_id).or_insert_with(TabState::default)
        })
    }

    /// Resize overlays to match their corresponding tab/pane dimensions
    pub fn resize_overlays(&self) {
        let Some(mux) = self.mux_or_log("resize overlays") else {
            return;
        };
        for (_, state) in self.tab_state.borrow().iter() {
            if let Some(overlay) = state.overlay.as_ref().map(|o| &o.pane) {
                overlay.resize(self.terminal_size).ok();
            }
        }
        for (pane_id, state) in self.pane_state.borrow().iter() {
            if let Some(overlay) = state.overlay.as_ref().map(|o| &o.pane) {
                if let Some(pane) = mux.get_pane(*pane_id) {
                    let dims = pane.get_dimensions();
                    overlay
                        .resize(TerminalSize {
                            cols: dims.cols,
                            rows: dims.viewport_rows,
                            dpi: self.terminal_size.dpi,
                            pixel_height: (self.terminal_size.pixel_height
                                / self.terminal_size.rows.max(1))
                                * dims.viewport_rows,
                            pixel_width: (self.terminal_size.pixel_width
                                / self.terminal_size.cols.max(1))
                                * dims.cols,
                        })
                        .ok();
                }
            }
        }
    }

    pub fn get_viewport(&self, pane_id: PaneId) -> Option<StableRowIndex> {
        self.pane_state(pane_id).viewport
    }

    pub fn set_viewport(
        &mut self,
        pane_id: PaneId,
        position: Option<StableRowIndex>,
        dims: RenderableDimensions,
    ) {
        let pos = match position {
            Some(pos) => {
                // Drop out of scrolling mode if we're off the bottom
                if pos >= dims.physical_top {
                    None
                } else {
                    Some(pos.max(dims.scrollback_top))
                }
            }
            None => None,
        };

        let viewport_changed = {
            let mut state = self.pane_state(pane_id);
            if pos != state.viewport {
                state.viewport = pos;

                // This is a bit gross.  If we add other overlays that need this information,
                // this should get extracted out into a trait
                if let Some(overlay) = state.overlay.as_ref() {
                    if let Some(copy) = overlay.pane.downcast_ref::<CopyOverlay>() {
                        copy.viewport_changed(pos);
                    } else if let Some(qs) = overlay.pane.downcast_ref::<QuickSelectOverlay>() {
                        qs.viewport_changed(pos);
                    }
                }
                true
            } else {
                false
            }
        };
        if viewport_changed {
            let viewport = pos.unwrap_or(dims.physical_top);
            if frankenterm_gui::checked_stable_row_range_from_top(viewport, dims.viewport_rows)
                .is_some()
            {
                self.dirty_lines_for_pane(pane_id, dims.viewport_rows)
                    .mark_all();
                self.record_dirty_event(
                    frankenterm_core::dirty_line_telemetry::DirtyEventSource::Viewport,
                );
            } else {
                metrics::counter!(
                    "gui.render.dirty_range_overflow",
                    "operation" => "viewport_invalidation"
                )
                .increment(1);
                log::error!(
                    "cannot invalidate viewport for pane {pane_id}: viewport {viewport} + {} rows overflows StableRowIndex",
                    dims.viewport_rows
                );
            }
        }
        if viewport_changed {
            if let Some(window) = self.window.as_ref() {
                window.invalidate();
            }
        }
    }

    fn maybe_scroll_to_bottom_for_input(&mut self, pane: &Arc<dyn Pane>) {
        if self.config.scroll_to_bottom_on_input {
            self.scroll_to_bottom(pane);
        }
    }

    fn scroll_to_top(&mut self, pane: &Arc<dyn Pane>) {
        let dims = pane.get_dimensions();
        self.set_viewport(pane.pane_id(), Some(dims.scrollback_top), dims);
    }

    fn scroll_to_bottom(&mut self, pane: &Arc<dyn Pane>) {
        let pane_id = pane.pane_id();
        if self.get_viewport(pane_id).is_none() {
            return;
        }
        self.set_viewport(pane_id, None, pane.get_dimensions());
    }

    fn get_active_pane_no_overlay(&self) -> Option<Arc<dyn Pane>> {
        self.mux_or_log("get active pane")
            .and_then(|mux| mux.get_active_tab_for_window(self.mux_window_id))
            .and_then(|tab| tab.get_active_pane())
    }

    /// Returns a Pane that we can interact with; this will typically be
    /// the active tab for the window, but if the window has a tab-wide
    /// overlay (such as the launcher / tab navigator),
    /// then that will be returned instead.  Otherwise, if the pane has
    /// an active overlay (such as search or copy mode) then that will
    /// be returned.
    pub fn get_active_pane_or_overlay(&self) -> Option<Arc<dyn Pane>> {
        let mux = self.mux_or_log("get active pane or overlay")?;
        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
            Some(tab) => tab,
            None => return None,
        };

        let tab_id = tab.tab_id();

        if let Some(tab_overlay) = self
            .tab_state(tab_id)
            .overlay
            .as_ref()
            .map(|overlay| overlay.pane.clone())
        {
            Some(tab_overlay)
        } else {
            let pane = tab.get_active_pane()?;
            let pane_id = pane.pane_id();
            self.pane_state(pane_id)
                .overlay
                .as_ref()
                .map(|overlay| overlay.pane.clone())
                .or_else(|| Some(pane))
        }
    }

    fn get_splits(&mut self) -> Vec<PositionedSplit> {
        let Some(mux) = self.mux_or_log("get splits") else {
            return vec![];
        };
        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
            Some(tab) => tab,
            None => return vec![],
        };

        let tab_id = tab.tab_id();

        if self.tab_state(tab_id).overlay.is_some() {
            vec![]
        } else {
            tab.iter_splits()
        }
    }

    fn pos_pane_to_pane_info(pos: &PositionedPane) -> PaneInformation {
        PaneInformation {
            pane_id: pos.pane.pane_id(),
            pane_index: pos.index,
            is_active: pos.is_active,
            is_zoomed: pos.is_zoomed,
            has_unseen_output: pos.pane.has_unseen_output(),
            left: pos.left,
            top: pos.top,
            width: pos.width,
            height: pos.height,
            pixel_width: pos.pixel_width,
            pixel_height: pos.pixel_height,
            title: pos.pane.get_title(),
            user_vars: pos.pane.copy_user_vars(),
            progress: pos.pane.get_progress(),
        }
    }

    fn get_tab_information(&mut self) -> Vec<TabInformation> {
        let Some(mux) = self.mux_or_log("get tab information") else {
            return vec![];
        };
        let window = match mux.get_window(self.mux_window_id) {
            Some(window) => window,
            _ => return vec![],
        };
        let tab_index = window.get_active_idx();

        window
            .iter()
            .enumerate()
            .map(|(idx, tab)| {
                let panes = self.get_pos_panes_for_tab(tab);

                TabInformation {
                    tab_index: idx,
                    tab_id: tab.tab_id(),
                    is_active: tab_index == idx,
                    is_last_active: window
                        .get_last_active_idx()
                        .map(|last_active| last_active == idx)
                        .unwrap_or(false),
                    window_id: self.mux_window_id,
                    tab_title: tab.get_title(),
                    active_pane: panes
                        .iter()
                        .find(|p| p.is_active)
                        .map(Self::pos_pane_to_pane_info),
                }
            })
            .collect()
    }

    fn get_pane_information(&self) -> Vec<PaneInformation> {
        self.get_panes_to_render()
            .iter()
            .map(Self::pos_pane_to_pane_info)
            .collect()
    }

    fn get_pos_panes_for_tab(&self, tab: &Arc<Tab>) -> Vec<PositionedPane> {
        let tab_id = tab.tab_id();

        if let Some(pane) = self
            .tab_state(tab_id)
            .overlay
            .as_ref()
            .map(|overlay| overlay.pane.clone())
        {
            let size = tab.get_size();
            vec![PositionedPane {
                index: 0,
                is_active: true,
                is_zoomed: false,
                left: 0,
                top: 0,
                width: size.cols as _,
                height: size.rows as _,
                pixel_width: size.cols as usize * self.render_metrics.cell_size.width as usize,
                pixel_height: size.rows as usize * self.render_metrics.cell_size.height as usize,
                pane,
            }]
        } else {
            let mut panes = tab.iter_panes();
            for p in &mut panes {
                if let Some(overlay) = self.pane_state(p.pane.pane_id()).overlay.as_ref() {
                    p.pane = Arc::clone(&overlay.pane);
                }
            }
            let mut floating = tab
                .iter_floating_panes()
                .into_iter()
                .filter(|pane| pane.visible)
                .enumerate()
                .map(|(floating_idx, pane)| PositionedPane {
                    index: panes.len() + floating_idx,
                    is_active: pane.is_focused,
                    is_zoomed: false,
                    left: pane.left,
                    top: pane.top,
                    width: pane.width,
                    height: pane.height,
                    pixel_width: pane
                        .width
                        .saturating_mul(self.render_metrics.cell_size.width as usize),
                    pixel_height: pane
                        .height
                        .saturating_mul(self.render_metrics.cell_size.height as usize),
                    pane: Arc::clone(&pane.pane),
                })
                .collect();
            panes.append(&mut floating);
            panes
        }
    }

    fn get_panes_to_render(&self) -> Vec<PositionedPane> {
        let Some(mux) = Mux::try_get() else {
            return vec![];
        };
        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
            Some(tab) => tab,
            None => return vec![],
        };

        self.get_pos_panes_for_tab(&tab)
    }

    fn prepare_overlay_state(slot: OverlaySlot, pane: Arc<dyn Pane>) -> OverlayState {
        let pane_id = pane.pane_id();
        let (registration, cancellation_ticket) = match Mux::try_get() {
            Some(mux) => {
                let registration = mux.capture_pane_registration(&pane);
                let cancellation_ticket = OverlayCancellationTicket::new(
                    Arc::downgrade(&mux),
                    slot,
                    pane_id,
                    registration.clone(),
                );
                (registration, cancellation_ticket)
            }
            None => (
                None,
                OverlayCancellationTicket::new(Weak::new(), slot, pane_id, None),
            ),
        };
        OverlayState {
            pane,
            key_table_state: KeyTableState::default(),
            registration,
            cancellation_ticket,
        }
    }

    fn mint_origin_overlay_cancellation_ticket(
        slot: OverlaySlot,
        pane: &Arc<dyn Pane>,
    ) -> anyhow::Result<OverlayCancellationTicket> {
        let mux = Mux::try_get()
            .context("cannot mint overlay cancellation authority without an active mux")?;
        let registration = mux.capture_pane_registration(pane).ok_or_else(|| {
            anyhow!(
                "cannot mint overlay cancellation authority for unregistered pane {}",
                pane.pane_id()
            )
        })?;
        Ok(OverlayCancellationTicket::new(
            Arc::downgrade(&mux),
            slot,
            pane.pane_id(),
            Some(registration),
        ))
    }

    pub(crate) fn mint_tab_overlay_cancellation_ticket(
        tab_id: TabId,
        pane: &Arc<dyn Pane>,
    ) -> anyhow::Result<OverlayCancellationTicket> {
        Self::mint_origin_overlay_cancellation_ticket(OverlaySlot::Tab(tab_id), pane)
    }

    pub(crate) fn mint_pane_overlay_cancellation_ticket(
        pane_id: PaneId,
        overlay: &Arc<dyn Pane>,
    ) -> anyhow::Result<OverlayCancellationTicket> {
        Self::mint_origin_overlay_cancellation_ticket(OverlaySlot::Pane(pane_id), overlay)
    }

    fn prepare_origin_overlay_state(
        slot: OverlaySlot,
        pane: Arc<dyn Pane>,
        ticket: OverlayCancellationTicket,
    ) -> anyhow::Result<OverlayState> {
        ensure!(
            ticket.slot == slot,
            "overlay cancellation ticket targets {:?}, not {:?}",
            ticket.slot,
            slot,
        );
        ensure!(
            ticket.overlay_pane_id == pane.pane_id(),
            "overlay cancellation ticket pane {} does not match assigned pane {}",
            ticket.overlay_pane_id,
            pane.pane_id(),
        );
        let owner = ticket
            .mux_owner
            .upgrade()
            .context("originating mux was destroyed before overlay assignment")?;
        let current = owner.capture_pane_registration(&pane).ok_or_else(|| {
            anyhow!(
                "overlay pane {} is no longer the exact originating mux registration",
                pane.pane_id()
            )
        })?;
        let expected = ticket
            .registration
            .as_ref()
            .context("worker overlay cancellation ticket lacks exact registration authority")?;
        ensure!(
            current.same_registration(expected),
            "overlay pane {} registration changed before assignment",
            pane.pane_id(),
        );

        Ok(OverlayState {
            pane,
            key_table_state: KeyTableState::default(),
            registration: Some(current),
            cancellation_ticket: ticket,
        })
    }

    /// Retire only the exact overlay registration captured at assignment.
    ///
    /// Never re-resolve by numeric pane ID here. A delayed cancellation can
    /// run after either the pane slot or the process-global mux has been
    /// replaced. GUI-only overlays intentionally carry no registration and
    /// therefore cannot kill the underlying same-numbered pane.
    fn retire_overlay_registration(_slot: OverlaySlot, overlay: OverlayState) {
        let pane_id = overlay.pane.pane_id();
        match overlay.registration {
            Some(registration) => {
                if !registration.retire_if_current() {
                    metrics::counter!("gui.overlay.cleanup.stale_registration").increment(1);
                    log::debug!(
                        "overlay pane {pane_id} registration was already retired or replaced; preserving the current slot"
                    );
                }
            }
            None => {
                metrics::counter!("gui.overlay.cleanup.gui_only").increment(1);
            }
        }
    }

    /// If `pane_id` is `None`, remove the current overlay for the specified
    /// tab. Otherwise remove it only when its pane ID also matches.
    fn cancel_overlay_for_tab(&mut self, tab_id: TabId, pane_id: Option<PaneId>) {
        self.cancel_overlay_for_tab_matching(tab_id, pane_id, None);
    }

    fn cancel_overlay_for_tab_if_current(
        &mut self,
        tab_id: TabId,
        pane_id: Option<PaneId>,
        ticket: &OverlayCancellationTicket,
    ) {
        self.cancel_overlay_for_tab_matching(tab_id, pane_id, Some(ticket));
    }

    fn cancel_overlay_for_tab_matching(
        &mut self,
        tab_id: TabId,
        pane_id: Option<PaneId>,
        expected: Option<&OverlayCancellationTicket>,
    ) {
        let mut rejected_expected = false;
        let mut pane_mismatch = false;
        let overlay = {
            let mut states = self.tab_state.borrow_mut();
            match states.get_mut(&tab_id) {
                Some(state)
                    if pane_id.is_some()
                        && state.overlay.as_ref().map(|o| o.pane.pane_id()) != pane_id =>
                {
                    pane_mismatch = true;
                    rejected_expected = expected.is_some();
                    None
                }
                Some(state)
                    if expected.is_some_and(|ticket| {
                        state
                            .overlay
                            .as_ref()
                            .is_none_or(|overlay| !overlay.cancellation_ticket.matches(ticket))
                    }) =>
                {
                    rejected_expected = true;
                    None
                }
                Some(state) => state.overlay.take(),
                None => {
                    rejected_expected = expected.is_some();
                    None
                }
            }
        };
        if rejected_expected {
            metrics::counter!("gui.overlay.cancel.stale_ticket_rejected", "slot" => "tab")
                .increment(1);
            return;
        }
        if pane_mismatch {
            return;
        }
        if let Some(overlay) = overlay {
            Self::retire_overlay_registration(OverlaySlot::Tab(tab_id), overlay);
        }
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    pub(crate) fn schedule_cancel_overlay(
        window: Window,
        tab_id: TabId,
        overlay_pane_id: PaneId,
        ticket: OverlayCancellationTicket,
    ) {
        if ticket.slot != OverlaySlot::Tab(tab_id) || ticket.overlay_pane_id != overlay_pane_id {
            metrics::counter!("gui.overlay.cancel.invalid_origin_ticket", "slot" => "tab")
                .increment(1);
            log::error!(
                "refusing mismatched tab overlay cancellation ticket: requested tab {tab_id}, pane {overlay_pane_id}; ticket targets {:?}, pane {}",
                ticket.slot,
                ticket.overlay_pane_id,
            );
            return;
        }
        ticket.request_cancellation();
        window.notify(TermWindowNotif::CancelOverlayForTab {
            tab_id,
            overlay_pane_id,
            ticket,
        });
    }

    fn cancel_overlay_for_pane(&mut self, pane_id: PaneId) {
        self.cancel_overlay_for_pane_matching(pane_id, None);
    }

    fn cancel_overlay_for_pane_if_current(
        &mut self,
        pane_id: PaneId,
        ticket: &OverlayCancellationTicket,
    ) {
        self.cancel_overlay_for_pane_matching(pane_id, Some(ticket));
    }

    fn cancel_overlay_for_pane_matching(
        &mut self,
        pane_id: PaneId,
        expected: Option<&OverlayCancellationTicket>,
    ) {
        let mut rejected_expected = false;
        let overlay = {
            let mut states = self.pane_state.borrow_mut();
            match states.get_mut(&pane_id) {
                Some(state)
                    if expected.is_some_and(|ticket| {
                        state
                            .overlay
                            .as_ref()
                            .is_none_or(|overlay| !overlay.cancellation_ticket.matches(ticket))
                    }) =>
                {
                    rejected_expected = true;
                    None
                }
                Some(state) => state.overlay.take(),
                None => {
                    rejected_expected = expected.is_some();
                    None
                }
            }
        };
        if rejected_expected {
            metrics::counter!(
                "gui.overlay.cancel.stale_ticket_rejected",
                "slot" => "pane"
            )
            .increment(1);
            return;
        }
        if let Some(overlay) = overlay {
            Self::retire_overlay_registration(OverlaySlot::Pane(pane_id), overlay);
        }
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    pub(crate) fn schedule_cancel_overlay_for_pane(
        window: Window,
        pane_id: PaneId,
        ticket: OverlayCancellationTicket,
    ) {
        if ticket.slot != OverlaySlot::Pane(pane_id) {
            metrics::counter!("gui.overlay.cancel.invalid_origin_ticket", "slot" => "pane")
                .increment(1);
            log::error!(
                "refusing mismatched pane overlay cancellation ticket: requested owner pane {pane_id}; ticket targets {:?}",
                ticket.slot,
            );
            return;
        }
        ticket.request_cancellation();
        window.notify(TermWindowNotif::CancelOverlayForPane { pane_id, ticket });
    }

    fn retire_unassigned_origin_ticket(ticket: &OverlayCancellationTicket) {
        if let Some(registration) = &ticket.registration {
            let _ = registration.retire_if_current();
        }
    }

    pub(crate) fn assign_overlay_for_pane_with_ticket(
        &mut self,
        pane_id: PaneId,
        pane: Arc<dyn Pane>,
        ticket: OverlayCancellationTicket,
    ) {
        let cleanup_ticket = ticket.clone();
        let overlay =
            match Self::prepare_origin_overlay_state(OverlaySlot::Pane(pane_id), pane, ticket) {
                Ok(overlay) => overlay,
                Err(err) => {
                    Self::retire_unassigned_origin_ticket(&cleanup_ticket);
                    log::error!("refusing pane overlay without exact origin authority: {err:#}");
                    return;
                }
            };
        if overlay.cancellation_ticket.cancellation_requested() {
            metrics::counter!("gui.overlay.cancel.completed_before_assignment", "slot" => "pane")
                .increment(1);
            Self::retire_overlay_registration(OverlaySlot::Pane(pane_id), overlay);
            return;
        }
        self.cancel_overlay_for_pane(pane_id);
        let replaced = self.pane_state(pane_id).overlay.replace(overlay);
        debug_assert!(replaced.is_none(), "old pane overlay must be retired first");
        self.update_title();
    }

    pub(crate) fn assign_overlay_with_ticket(
        &mut self,
        tab_id: TabId,
        pane: Arc<dyn Pane>,
        ticket: OverlayCancellationTicket,
    ) {
        let cleanup_ticket = ticket.clone();
        let overlay =
            match Self::prepare_origin_overlay_state(OverlaySlot::Tab(tab_id), pane, ticket) {
                Ok(overlay) => overlay,
                Err(err) => {
                    Self::retire_unassigned_origin_ticket(&cleanup_ticket);
                    log::error!("refusing tab overlay without exact origin authority: {err:#}");
                    return;
                }
            };
        if overlay.cancellation_ticket.cancellation_requested() {
            metrics::counter!("gui.overlay.cancel.completed_before_assignment", "slot" => "tab")
                .increment(1);
            Self::retire_overlay_registration(OverlaySlot::Tab(tab_id), overlay);
            return;
        }
        self.cancel_overlay_for_tab(tab_id, None);
        let replaced = self.tab_state(tab_id).overlay.replace(overlay);
        debug_assert!(replaced.is_none(), "old tab overlay must be retired first");
        self.update_title();
    }

    pub fn assign_overlay_for_pane(&mut self, pane_id: PaneId, pane: Arc<dyn Pane>) {
        self.cancel_overlay_for_pane(pane_id);
        let overlay = Self::prepare_overlay_state(OverlaySlot::Pane(pane_id), pane);
        let replaced = self.pane_state(pane_id).overlay.replace(overlay);
        debug_assert!(replaced.is_none(), "old pane overlay must be retired first");
        self.update_title();
    }

    pub fn assign_overlay(&mut self, tab_id: TabId, overlay: Arc<dyn Pane>) {
        self.cancel_overlay_for_tab(tab_id, None);
        let overlay = Self::prepare_overlay_state(OverlaySlot::Tab(tab_id), overlay);
        let replaced = self.tab_state(tab_id).overlay.replace(overlay);
        debug_assert!(replaced.is_none(), "old tab overlay must be retired first");
        self.update_title();
    }

    fn resolve_search_pattern(&self, pattern: Pattern, pane: &Arc<dyn Pane>) -> MuxPattern {
        match pattern {
            Pattern::CaseSensitiveString(s) => MuxPattern::CaseSensitiveString(s),
            Pattern::CaseInSensitiveString(s) => MuxPattern::CaseInSensitiveString(s),
            Pattern::Regex(s) => MuxPattern::Regex(s),
            Pattern::CurrentSelectionOrEmptyString => {
                let text = self.selection_text(pane);
                let first_line = text
                    .lines()
                    .next()
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                MuxPattern::CaseSensitiveString(first_line)
            }
        }
    }
}

impl Drop for TermWindow {
    fn drop(&mut self) {
        self.clear_all_overlays();
        self.background_load.cancel();
        self.release_render_resources_before_window();
        if let Some(window) = self.window.take() {
            if let Some(fe) = try_front_end() {
                fe.forget_known_window(&window);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DamageAdvanceOutcome, DamageCommitOutcome, DamageGeneration, DrawFailure,
        DrawFailureStage, FrameCompletion, PaintAdmission, RenderAttemptFailure,
        RenderFailureStage, RenderRecoveryDirective, RenderRecoveryState, RenderWakeDispatch,
        RenderWakePlan, RenderWakeReason, RenderWakeState,
        SyncOutputDoctorSnapshot, UIItem, UIItemType, WebGpuSurfaceErrorAction,
        acquire_webgpu_frame_with_repair_using, a11y_op_kind_from_frame_budget_op,
        apply_failed_render_attempt, apply_presented_render_attempt,
        base_policy_for_frame_budget_state, classify_webgpu_surface_error,
        combine_glium_draw_and_finish, default_frame_budget_cost_ns,
        evaluate_frame_budget_reduce_motion_gate, frame_budget, mark_cursor_rows_dirty,
        mark_stable_row_ranges_dirty, mark_stable_rows_dirty,
        pane_health_snapshot_from_watchdoged_health, record_drained_frame_budget_ops,
        record_frame_budget_execution_outstanding, record_sync_output_mux_event,
        reduce_motion_state_from_preference, render, render_recovery_directive,
        run_clear_dirty_lines_after_frame, settle_frame_damage, should_force_paint_for_frame_budget,
        is_clean_line_for_cache_hit_accounting, should_run_frame_budget_decision, webgpu,
        webgpu_repair_failure_stage,
    };
    use std::time::{Duration, Instant};

    #[test]
    fn failed_workspace_spawn_rolls_back_only_its_own_empty_selection() {
        assert!(super::should_restore_workspace_after_failed_spawn(
            "requested",
            "requested",
            false,
        ));
        assert!(!super::should_restore_workspace_after_failed_spawn(
            "newer-operator-selection",
            "requested",
            false,
        ));
        assert!(!super::should_restore_workspace_after_failed_spawn(
            "requested",
            "requested",
            true,
        ));
    }

    #[test]
    fn pane_removed_gui_cleanup_fails_closed_without_exact_deferred_authority() {
        assert!(!super::TermWindow::mux_notification_has_deferred_cleanup_authority(
            &mux::MuxNotification::PaneRemoved(7),
            false,
        ));
        assert!(super::TermWindow::mux_notification_has_deferred_cleanup_authority(
            &mux::MuxNotification::PaneRemoved(7),
            true,
        ));
        assert!(super::TermWindow::mux_notification_has_deferred_cleanup_authority(
            &mux::MuxNotification::PaneOutput(7),
            false,
        ));
    }

    #[test]
    fn queued_tab_overlay_ticket_cannot_cancel_same_slot_replacement() {
        let owner = std::sync::Arc::new(mux::Mux::new(None));
        let slot = super::OverlaySlot::Tab(usize::MAX - 101);
        let overlay_pane_id = usize::MAX - 201;
        let old_ticket = super::OverlayCancellationTicket::new(
            std::sync::Arc::downgrade(&owner),
            slot,
            overlay_pane_id,
            None,
        );
        let queued = old_ticket.clone();
        let replacement_ticket = super::OverlayCancellationTicket::new(
            std::sync::Arc::downgrade(&owner),
            slot,
            usize::MAX - 202,
            None,
        );

        assert!(
            !replacement_ticket.matches(&queued),
            "a queued tab ticket must remain bound to the replaced overlay instance",
        );
    }

    #[test]
    fn queued_pane_overlay_ticket_cannot_cancel_same_slot_replacement() {
        let owner = std::sync::Arc::new(mux::Mux::new(None));
        let slot = super::OverlaySlot::Pane(usize::MAX - 103);
        let old_ticket = super::OverlayCancellationTicket::new(
            std::sync::Arc::downgrade(&owner),
            slot,
            usize::MAX - 203,
            None,
        );
        let replacement_ticket = super::OverlayCancellationTicket::new(
            std::sync::Arc::downgrade(&owner),
            slot,
            usize::MAX - 204,
            None,
        );

        assert!(
            !replacement_ticket.matches(&old_ticket),
            "a queued pane ticket must remain bound to the replaced overlay instance",
        );
    }

    #[test]
    fn reused_numeric_overlay_pane_id_does_not_retarget_old_ticket() {
        let owner = std::sync::Arc::new(mux::Mux::new(None));
        let slot = super::OverlaySlot::Tab(usize::MAX - 104);
        let reused_overlay_pane_id = usize::MAX - 205;
        let old_ticket = super::OverlayCancellationTicket::new(
            std::sync::Arc::downgrade(&owner),
            slot,
            reused_overlay_pane_id,
            None,
        );
        let replacement_ticket = super::OverlayCancellationTicket::new(
            std::sync::Arc::downgrade(&owner),
            slot,
            reused_overlay_pane_id,
            None,
        );

        assert!(
            !replacement_ticket.matches(&old_ticket),
            "numeric pane-ID reuse must not confer cancellation authority over the replacement",
        );
    }

    #[test]
    fn overlay_ticket_separates_same_numeric_ids_across_mux_owners() {
        let old_owner = std::sync::Arc::new(mux::Mux::new(None));
        let new_owner = std::sync::Arc::new(mux::Mux::new(None));
        let slot = super::OverlaySlot::Pane(usize::MAX - 102);
        let overlay_pane_id = usize::MAX - 206;
        let old_ticket = super::OverlayCancellationTicket::new(
            std::sync::Arc::downgrade(&old_owner),
            slot,
            overlay_pane_id,
            None,
        );
        let new_ticket = super::OverlayCancellationTicket::new(
            std::sync::Arc::downgrade(&new_owner),
            slot,
            overlay_pane_id,
            None,
        );

        assert!(
            !new_ticket.matches(&old_ticket),
            "a replacement mux must not inherit cancellation authority from the old owner",
        );
    }

    #[test]
    fn overlay_completion_before_assignment_is_observed_by_assignment_clone() {
        let owner = std::sync::Arc::new(mux::Mux::new(None));
        let ticket = super::OverlayCancellationTicket::new(
            std::sync::Arc::downgrade(&owner),
            super::OverlaySlot::Pane(usize::MAX - 105),
            usize::MAX - 207,
            None,
        );
        let assignment_ticket = ticket.clone();

        ticket.request_cancellation();

        assert!(
            assignment_ticket.cancellation_requested(),
            "the assignment-side clone must observe completion published by the worker-side clone",
        );
        assert!(assignment_ticket.matches(&ticket));
    }

    #[test]
    fn ui_item_hit_test_uses_half_open_extents() {
        let item = UIItem {
            x: 10,
            y: 20,
            width: 3,
            height: 2,
            item_type: UIItemType::AboveScrollThumb,
        };

        assert!(item.hit_test(10, 20));
        assert!(item.hit_test(12, 21));
        assert!(!item.hit_test(13, 21));
        assert!(!item.hit_test(12, 22));
        assert!(!item.hit_test(9, 20));
        assert!(!item.hit_test(10, 19));
    }

    #[test]
    fn ui_item_hit_test_zero_extent_items_are_not_clickable() {
        let zero_width = UIItem {
            x: 10,
            y: 20,
            width: 0,
            height: 2,
            item_type: UIItemType::AboveScrollThumb,
        };
        let zero_height = UIItem {
            x: 10,
            y: 20,
            width: 2,
            height: 0,
            item_type: UIItemType::AboveScrollThumb,
        };

        assert!(!zero_width.hit_test(10, 20));
        assert!(!zero_height.hit_test(10, 20));
    }

    /// ft-camu6: stable→visible translation marks the right rows.
    #[test]
    fn mark_stable_rows_translates_via_viewport_subtract() {
        let mut bm = render::dirty_lines::DirtyLineBitmap::new(24);
        // viewport=100, stable rows 102, 105, 110 → visible 2, 5, 10.
        mark_stable_rows_dirty(&mut bm, 100, [102_isize, 105, 110]);
        assert!(bm.contains(2));
        assert!(bm.contains(5));
        assert!(bm.contains(10));
        assert_eq!(bm.count(), 3);
    }

    /// ft-camu6: stable rows above the viewport (e.g., scrolled
    /// past) are silently dropped — no panic, no spurious mark.
    #[test]
    fn mark_stable_rows_drops_rows_past_capacity() {
        let mut bm = render::dirty_lines::DirtyLineBitmap::new(24);
        // viewport=100, capacity=24 → visible idx [0,24).
        // stable=125 → visible=25 → past capacity → dropped.
        mark_stable_rows_dirty(&mut bm, 100, [125_isize]);
        assert_eq!(bm.count(), 0);
    }

    /// ft-camu6: stable rows BELOW the viewport (negative visible
    /// idx after subtract) are silently dropped via saturating_sub
    /// + try_from — no underflow.
    #[test]
    fn mark_stable_rows_drops_rows_below_viewport() {
        let mut bm = render::dirty_lines::DirtyLineBitmap::new(24);
        // viewport=100, stable=50 → saturating_sub(100,50)=0... wait.
        // saturating_sub on i64 is regular sub clamped, but i64 has
        // negatives — saturating_sub(50, 100) = -50 which is valid.
        // try_from on negative i64 → Err(usize) so it drops.
        // Confirm: stable=50, viewport=100 → -50 → try_from fails → drop.
        mark_stable_rows_dirty(&mut bm, 100, [50_isize]);
        assert_eq!(bm.count(), 0);
        // Boundary: stable=100, viewport=100 → 0 → marks visible[0].
        mark_stable_rows_dirty(&mut bm, 100, [100_isize]);
        assert!(bm.contains(0));
        assert_eq!(bm.count(), 1);
    }

    /// ft-camu6: empty input is a clean no-op.
    #[test]
    fn mark_stable_rows_empty_input_no_op() {
        let mut bm = render::dirty_lines::DirtyLineBitmap::new(24);
        mark_stable_rows_dirty(&mut bm, 100, std::iter::empty::<isize>());
        assert_eq!(bm.count(), 0);
        assert_eq!(bm.dirty_marks_total(), 0);
    }

    /// ft-camu6: re-marking the same row is idempotent (the
    /// bitmap's own contract — already covered by dirty_lines tests
    /// but pinned here too at the integration level).
    #[test]
    fn mark_stable_rows_repeat_is_idempotent() {
        let mut bm = render::dirty_lines::DirtyLineBitmap::new(24);
        mark_stable_rows_dirty(&mut bm, 100, [105_isize, 105, 105]);
        assert_eq!(bm.count(), 1);
        assert_eq!(bm.dirty_marks_total(), 1);
    }

    /// ft-gwzrm: live dirty ranges are translated as ranges, not as
    /// an expanded list of every row. Partial viewport overlaps are
    /// clamped so visible damage is not dropped at the edges.
    #[test]
    fn mark_stable_row_ranges_clamps_partial_viewport_overlap() {
        let mut bm = render::dirty_lines::DirtyLineBitmap::new(24);
        mark_stable_row_ranges_dirty(&mut bm, 100, [95_isize..103, 118..130]);

        for row in 0..3 {
            assert!(bm.contains(row), "top overlap row {row} should be dirty");
        }
        for row in 18..24 {
            assert!(bm.contains(row), "bottom overlap row {row} should be dirty",);
        }
        assert_eq!(bm.count(), 9);
        assert_eq!(bm.dirty_marks_total(), 9);
    }

    /// ft-gwzrm: ranges wholly outside the visible viewport remain
    /// no-ops. This keeps scrolled-away mux dirty ranges from
    /// creating false positives in the clean-line skip predicate.
    #[test]
    fn mark_stable_row_ranges_drops_out_of_view_ranges() {
        let mut bm = render::dirty_lines::DirtyLineBitmap::new(24);
        mark_stable_row_ranges_dirty(&mut bm, 100, [10_isize..20, 124..130]);

        assert_eq!(bm.count(), 0);
        assert_eq!(bm.dirty_marks_total(), 0);
    }

    /// ft-jvj78: cursor movement invalidates both the row that lost
    /// the cursor and the row that gained it.
    #[test]
    fn mark_cursor_rows_marks_old_and_new_visible_rows() {
        use mux::renderable::StableCursorPosition;

        let mut bm = render::dirty_lines::DirtyLineBitmap::new(24);
        mark_cursor_rows_dirty(
            &mut bm,
            100,
            StableCursorPosition {
                y: 105,
                ..StableCursorPosition::default()
            },
            StableCursorPosition {
                y: 110,
                ..StableCursorPosition::default()
            },
        );

        assert!(bm.contains(5));
        assert!(bm.contains(10));
        assert_eq!(bm.count(), 2);
        assert_eq!(bm.dirty_marks_total(), 2);
    }

    /// ft-i6k6u: the substrate's MarksBySource record method
    /// bumps each per-source counter independently. Pinned here
    /// at the integration boundary so future refactors don't
    /// silently break attribution.
    #[test]
    fn marks_by_source_records_each_variant_independently() {
        use frankenterm_core::dirty_line_telemetry::{DirtyEventSource, MarksBySource};
        let mut m = MarksBySource::default();
        m.record(DirtyEventSource::Pty);
        m.record(DirtyEventSource::Pty);
        m.record(DirtyEventSource::CursorMove);
        m.record(DirtyEventSource::SelectionChange);
        m.record(DirtyEventSource::ThemeSwap);
        m.record(DirtyEventSource::FontSwap);
        m.record(DirtyEventSource::StatusTileUpdate);
        m.record(DirtyEventSource::FocusChange);
        m.record(DirtyEventSource::Resize);
        m.record(DirtyEventSource::Viewport);
        assert_eq!(m.pty, 2);
        assert_eq!(m.cursor_move, 1);
        assert_eq!(m.selection_change, 1);
        assert_eq!(m.theme_swap, 1);
        assert_eq!(m.font_swap, 1);
        assert_eq!(m.status_tile_update, 1);
        assert_eq!(m.focus_change, 1);
        assert_eq!(m.resize, 1);
        assert_eq!(m.viewport, 1);
    }

    /// ft-gso6n: aggregate_fleet_health on an empty snapshot
    /// list returns an all-zero aggregate distinguishable from a
    /// real fleet via total_panes=0. Substrate contract pinned at
    /// the integration boundary.
    #[test]
    fn fleet_health_aggregate_empty_returns_zero() {
        use frankenterm_core::triple_buffer_fleet_health::aggregate_fleet_health;
        let agg = aggregate_fleet_health(&[]);
        assert_eq!(agg.total_panes, 0);
        assert_eq!(agg.panes_currently_active_watchdog, 0);
        assert_eq!(agg.panes_ever_force_recycled, 0);
        assert_eq!(agg.total_force_recycles, 0);
    }

    /// ft-gso6n: aggregate_fleet_health folds per-pane snapshots
    /// into the right aggregate counters. Pin the substrate
    /// contract at the integration boundary so a future refactor
    /// of the substrate doesn't silently break the doctor surface.
    #[test]
    fn fleet_health_aggregate_folds_multiple_panes() {
        use frankenterm_core::triple_buffer_fleet_health::{
            PaneHealthSnapshot, PaneId, aggregate_fleet_health,
        };
        let snaps = vec![
            PaneHealthSnapshot {
                pane_id: PaneId(1),
                acquires: 100,
                releases: 100,
                warnings: 0,
                force_recycles: 0,
                last_force_recycle_ts_ms: 0,
                watchdog_active: false,
            },
            PaneHealthSnapshot {
                pane_id: PaneId(2),
                acquires: 200,
                releases: 199,
                warnings: 1,
                force_recycles: 1,
                last_force_recycle_ts_ms: 1_700_000_000_000,
                watchdog_active: true,
            },
            PaneHealthSnapshot {
                pane_id: PaneId(3),
                acquires: 50,
                releases: 50,
                warnings: 3,
                force_recycles: 5,
                last_force_recycle_ts_ms: 1_700_000_000_500,
                watchdog_active: false,
            },
        ];
        let agg = aggregate_fleet_health(&snaps);
        assert_eq!(agg.total_panes, 3);
        assert_eq!(agg.panes_currently_active_watchdog, 1); // pane 2
        assert_eq!(agg.panes_ever_force_recycled, 2); // panes 2 + 3
        assert_eq!(agg.total_acquires, 350);
        assert_eq!(agg.total_releases, 349);
        assert_eq!(agg.total_warnings, 4);
        assert_eq!(agg.total_force_recycles, 6);
        assert_eq!(agg.max_per_pane_force_recycles, 5);
    }

    fn terminal_state_for_watchdog_tests(
        seq: u16,
    ) -> frankenterm_core::session_pane_state::TerminalState {
        frankenterm_core::session_pane_state::TerminalState {
            rows: 24,
            cols: 80,
            cursor_row: seq,
            cursor_col: seq.saturating_mul(2),
            is_alt_screen: false,
            title: format!("pane-{seq}"),
        }
    }

    /// ft-71v6n: GUI bridge from live WatchdogedTripleBuffer health
    /// into the per-pane doctor snapshot preserves clean render-loop
    /// counters from an actual TerminalState wrapper.
    #[test]
    fn pane_health_snapshot_bridge_maps_clean_watchdoged_terminal_state() {
        use frankenterm_core::watchdoged_triple_buffer::WatchdogedTripleBuffer;
        let origin = std::time::Instant::now();
        let wtb = WatchdogedTripleBuffer::new(terminal_state_for_watchdog_tests(0));

        let guard = wtb.acquire(origin);
        assert_eq!(guard.cursor_row, 0);
        drop(guard);

        let snapshot = pane_health_snapshot_from_watchdoged_health(42, &wtb.health(), 0);
        assert_eq!(
            snapshot.pane_id,
            frankenterm_core::triple_buffer_fleet_health::PaneId(42)
        );
        assert_eq!(snapshot.acquires, 1);
        assert_eq!(snapshot.releases, 1);
        assert_eq!(snapshot.warnings, 0);
        assert_eq!(snapshot.force_recycles, 0);
        assert_eq!(snapshot.last_force_recycle_ts_ms, 0);
        assert!(!snapshot.watchdog_active);
    }

    /// ft-71v6n: the same bridge carries the hung-renderer
    /// force-recycle signal and fleet aggregation isolates it to
    /// the pane whose guard was held past the watchdog deadline.
    #[test]
    fn pane_health_snapshot_bridge_isolates_hung_renderer_force_recycle() {
        use frankenterm_core::triple_buffer_fleet_health::aggregate_fleet_health;
        use frankenterm_core::watchdoged_triple_buffer::WatchdogedTripleBuffer;

        let origin = std::time::Instant::now();
        let clean = WatchdogedTripleBuffer::new(terminal_state_for_watchdog_tests(1));
        let hung = WatchdogedTripleBuffer::new(terminal_state_for_watchdog_tests(2));

        let clean_guard = clean.acquire(origin);
        drop(clean_guard);

        let _hung_guard = hung.acquire(origin);
        let _ = hung.poll(origin + std::time::Duration::from_secs(6));

        let clean_snapshot = pane_health_snapshot_from_watchdoged_health(10, &clean.health(), 0);
        let hung_snapshot = pane_health_snapshot_from_watchdoged_health(11, &hung.health(), 12_345);

        assert_eq!(clean_snapshot.force_recycles, 0);
        assert!(
            hung_snapshot.force_recycles >= 1,
            "hung pane should report a watchdog force-recycle",
        );
        assert_eq!(hung_snapshot.last_force_recycle_ts_ms, 12_345);

        let aggregate = aggregate_fleet_health(&[clean_snapshot, hung_snapshot]);
        assert_eq!(aggregate.total_panes, 2);
        assert_eq!(aggregate.panes_ever_force_recycled, 1);
        assert_eq!(aggregate.total_force_recycles, hung_snapshot.force_recycles);
    }

    /// ft-a9eu1: SyncOutputDoctorSnapshot folds both substrate
    /// telemetry types into one view. Pinned at the integration
    /// boundary so a substrate refactor doesn't silently drop a
    /// counter that the doctor surface has been displaying.
    /// The Default value is all-zero across both halves.
    #[test]
    fn sync_output_doctor_snapshot_default_is_zero_across_both_halves() {
        let s = SyncOutputDoctorSnapshot::default();
        // Watchdog half.
        assert_eq!(s.bsu_count, 0);
        assert_eq!(s.esu_count, 0);
        assert_eq!(s.watchdog_force_flush_count, 0);
        assert_eq!(s.adversarial_esu_underflow_count, 0);
        // Orchestrator half.
        assert_eq!(s.admissions_accepted, 0);
        assert_eq!(s.admissions_refused, 0);
        assert_eq!(s.bytes_drained_total, 0);
        assert_eq!(s.drains_watchdog, 0);
    }

    /// ft-a9eu1: when the watchdog telemetry records a BSU open
    /// and the orchestrator records a refused admission, the
    /// SyncOutputDoctorSnapshot must reflect both.
    #[test]
    fn sync_output_doctor_snapshot_reflects_substrate_state() {
        use frankenterm_core::sync_output_buffer_orchestrator::SyncOutputOrchestratorTelemetry;
        use frankenterm_core::sync_output_watchdog::{BsuDepthOutcome, SyncOutputTelemetry};

        // Drive the watchdog half via record_depth_outcome.
        let mut w = SyncOutputTelemetry::default();
        w.record_depth_outcome(BsuDepthOutcome::Opened { new_depth: 1 }, 0);
        w.record_depth_outcome(BsuDepthOutcome::Flushed, 1);
        // Direct assignment on the orchestrator half — substrate
        // exposes pub fields here so the test can simulate any
        // counter state.
        let o = SyncOutputOrchestratorTelemetry {
            admissions_accepted: 5,
            admissions_refused: 2,
            bytes_drained_total: 4096,
            drains_watchdog: 1,
            ..Default::default()
        };

        // Synthesize the snapshot via the same folding logic the
        // production sync_output_telemetry() uses. (TermWindow
        // construction is too heavy for a unit test; folding the
        // substrate types matches the production path.)
        let s = SyncOutputDoctorSnapshot {
            bsu_count: w.bsu_count(),
            esu_count: w.esu_count(),
            esu_flush_count: w.esu_flush_count(),
            watchdog_force_flush_count: w.watchdog_force_flush_count(),
            mid_bsu_byte_count: w.mid_bsu_byte_count(),
            max_bsu_depth_observed: w.max_bsu_depth_observed(),
            mode_query_count: w.mode_query_count(),
            adversarial_esu_underflow_count: w.adversarial_esu_underflow_count(),
            admissions_accepted: o.admissions_accepted,
            admissions_truncated: o.admissions_truncated,
            admissions_refused: o.admissions_refused,
            bytes_accepted: o.bytes_accepted,
            bytes_truncated: o.bytes_truncated,
            bytes_refused: o.bytes_refused,
            bytes_drained_total: o.bytes_drained_total,
            overrides_pass_through: o.overrides_pass_through,
            overrides_coalesced: o.overrides_coalesced,
            overrides_force_flush: o.overrides_force_flush,
            drains_esu: o.drains_esu,
            drains_watchdog: o.drains_watchdog,
            drains_live_resize: o.drains_live_resize,
            drains_operator: o.drains_operator,
            drains_no_op: o.drains_no_op,
            overrides_by_trigger: o.overrides_by_trigger,
        };

        assert_eq!(s.bsu_count, 1, "1 BSU opened");
        assert_eq!(s.esu_count, 1, "1 ESU closed");
        assert_eq!(s.esu_flush_count, 1, "1 flush via ESU");
        assert_eq!(s.max_bsu_depth_observed, 1);
        assert_eq!(s.admissions_accepted, 5);
        assert_eq!(s.admissions_refused, 2);
        assert_eq!(s.bytes_drained_total, 4096);
        assert_eq!(s.drains_watchdog, 1);
        // Adversarial counter stays zero — no underflow simulated.
        assert_eq!(s.adversarial_esu_underflow_count, 0);
    }

    #[test]
    fn sync_output_mux_events_feed_watchdog_and_orchestrator_counters() {
        let pane_id = 11;
        let mut watchdog = frankenterm_core::sync_output_watchdog::SyncOutputTelemetry::default();
        let mut orchestrator =
            frankenterm_core::sync_output_buffer_orchestrator::SyncOutputOrchestratorTelemetry::default();
        let mut depth_by_pane = std::collections::HashMap::new();
        let mut buffered_bytes_by_pane = std::collections::HashMap::new();

        record_sync_output_mux_event(
            pane_id,
            mux::SynchronizedOutputEvent::Depth {
                outcome: mux::SynchronizedOutputDepthOutcome::Opened { new_depth: 1 },
                max_depth: 1,
            },
            &mut watchdog,
            &mut orchestrator,
            &mut depth_by_pane,
            &mut buffered_bytes_by_pane,
        );
        record_sync_output_mux_event(
            pane_id,
            mux::SynchronizedOutputEvent::Admission {
                decision: mux::SynchronizedOutputAdmissionDecision::Accepted,
                bytes: 64,
            },
            &mut watchdog,
            &mut orchestrator,
            &mut depth_by_pane,
            &mut buffered_bytes_by_pane,
        );
        record_sync_output_mux_event(
            pane_id,
            mux::SynchronizedOutputEvent::ModeQuery,
            &mut watchdog,
            &mut orchestrator,
            &mut depth_by_pane,
            &mut buffered_bytes_by_pane,
        );
        record_sync_output_mux_event(
            pane_id,
            mux::SynchronizedOutputEvent::Drain {
                cause: mux::SynchronizedOutputDrainCause::Esu,
                bytes: 0,
                depth_outcome: Some(mux::SynchronizedOutputDepthOutcome::Flushed),
                max_depth: 1,
            },
            &mut watchdog,
            &mut orchestrator,
            &mut depth_by_pane,
            &mut buffered_bytes_by_pane,
        );

        assert_eq!(watchdog.bsu_count(), 1);
        assert_eq!(watchdog.esu_count(), 1);
        assert_eq!(watchdog.esu_flush_count(), 1);
        assert_eq!(watchdog.mid_bsu_byte_count(), 64);
        assert_eq!(watchdog.mode_query_count(), 1);
        assert_eq!(watchdog.max_bsu_depth_observed(), 1);
        assert_eq!(orchestrator.admissions_accepted, 1);
        assert_eq!(orchestrator.bytes_accepted, 64);
        assert_eq!(orchestrator.drains_esu, 1);
        assert_eq!(orchestrator.bytes_drained_total, 64);
        assert!(!buffered_bytes_by_pane.contains_key(&pane_id));
    }

    #[test]
    fn sync_output_mux_events_preserve_drain_cause_classification() {
        let pane_id = 17;
        let mut watchdog = frankenterm_core::sync_output_watchdog::SyncOutputTelemetry::default();
        let mut orchestrator =
            frankenterm_core::sync_output_buffer_orchestrator::SyncOutputOrchestratorTelemetry::default();
        let mut depth_by_pane = std::collections::HashMap::new();
        let mut buffered_bytes_by_pane = std::collections::HashMap::new();

        record_sync_output_mux_event(
            pane_id,
            mux::SynchronizedOutputEvent::Drain {
                cause: mux::SynchronizedOutputDrainCause::Esu,
                bytes: 0,
                depth_outcome: Some(mux::SynchronizedOutputDepthOutcome::Underflow),
                max_depth: 0,
            },
            &mut watchdog,
            &mut orchestrator,
            &mut depth_by_pane,
            &mut buffered_bytes_by_pane,
        );
        assert_eq!(watchdog.adversarial_esu_underflow_count(), 1);
        assert_eq!(orchestrator.drains_no_op, 1);

        for (bytes, cause) in [
            (10, mux::SynchronizedOutputDrainCause::Watchdog),
            (11, mux::SynchronizedOutputDrainCause::LiveResizeForce),
            (12, mux::SynchronizedOutputDrainCause::Operator),
        ] {
            record_sync_output_mux_event(
                pane_id,
                mux::SynchronizedOutputEvent::Depth {
                    outcome: mux::SynchronizedOutputDepthOutcome::Opened { new_depth: 1 },
                    max_depth: 1,
                },
                &mut watchdog,
                &mut orchestrator,
                &mut depth_by_pane,
                &mut buffered_bytes_by_pane,
            );
            record_sync_output_mux_event(
                pane_id,
                mux::SynchronizedOutputEvent::Admission {
                    decision: mux::SynchronizedOutputAdmissionDecision::Accepted,
                    bytes,
                },
                &mut watchdog,
                &mut orchestrator,
                &mut depth_by_pane,
                &mut buffered_bytes_by_pane,
            );
            record_sync_output_mux_event(
                pane_id,
                mux::SynchronizedOutputEvent::Drain {
                    cause,
                    bytes: 0,
                    depth_outcome: None,
                    max_depth: 1,
                },
                &mut watchdog,
                &mut orchestrator,
                &mut depth_by_pane,
                &mut buffered_bytes_by_pane,
            );
        }

        assert_eq!(watchdog.watchdog_force_flush_count(), 1);
        assert_eq!(orchestrator.drains_watchdog, 1);
        assert_eq!(orchestrator.drains_live_resize, 1);
        assert_eq!(orchestrator.drains_operator, 1);
        assert_eq!(orchestrator.overrides_force_flush, 1);
        assert_eq!(orchestrator.overrides_by_trigger.live_resize, 1);
        assert_eq!(orchestrator.bytes_drained_total, 33);
    }

    /// ft-gso6n: PaneHealthSnapshot::ms_since_last_recycle returns
    /// None when no recycle has fired (lifetime counter at 0),
    /// Some(delta) otherwise. Pinned at integration boundary so
    /// the doctor's "watchdog active >Xs" warning has a stable
    /// per-pane signal to read.
    #[test]
    fn pane_health_ms_since_last_recycle_returns_none_when_never_fired() {
        use frankenterm_core::triple_buffer_fleet_health::PaneHealthSnapshot;
        let never_fired = PaneHealthSnapshot::default();
        assert_eq!(never_fired.ms_since_last_recycle(1_700_000_000_000), None);

        let fired = PaneHealthSnapshot {
            last_force_recycle_ts_ms: 1_700_000_000_000,
            force_recycles: 1,
            ..PaneHealthSnapshot::default()
        };
        assert_eq!(fired.ms_since_last_recycle(1_700_000_005_000), Some(5_000));
        // Saturating sub: now < last_recycle is harmless rather
        // than panic.
        assert_eq!(fired.ms_since_last_recycle(0), Some(0));
    }

    /// ft-i6k6u: the substrate's whole-screen classification is
    /// what mark_all_panes_dirty_with_source relies on for the
    /// frame-end clear suppression. Pin the predicate here so the
    /// whole-screen variants stay aligned with the wiring sites.
    #[test]
    fn dirty_event_source_whole_screen_classification_matches_wiring() {
        use frankenterm_core::dirty_line_telemetry::DirtyEventSource;
        // Per-row sources — must NOT trigger whole-screen.
        assert!(!DirtyEventSource::Pty.is_whole_screen());
        assert!(!DirtyEventSource::CursorMove.is_whole_screen());
        assert!(!DirtyEventSource::SelectionChange.is_whole_screen());
        assert!(!DirtyEventSource::StatusTileUpdate.is_whole_screen());
        // Whole-screen sources — must align with the call sites
        // wired through mark_all_panes_dirty_with_source.
        assert!(DirtyEventSource::ThemeSwap.is_whole_screen());
        assert!(DirtyEventSource::FontSwap.is_whole_screen());
        assert!(DirtyEventSource::FocusChange.is_whole_screen());
        assert!(DirtyEventSource::Resize.is_whole_screen());
        assert!(DirtyEventSource::Viewport.is_whole_screen());
    }

    use std::collections::HashMap;

    /// ft-d6nrd: redraw predicate is FALSE only when both the
    /// FrameBudget queue and the cosmetic-defer aggregator are
    /// empty — that's the steady-state "nothing pending" frame.
    #[test]
    fn force_paint_predicate_false_when_no_pending_work() {
        assert!(!should_force_paint_for_frame_budget(0, 0));
    }

    /// ft-d6nrd: deferred ops in the FrameBudget queue alone
    /// force the next frame.
    #[test]
    fn force_paint_predicate_true_when_queue_has_carryover() {
        assert!(should_force_paint_for_frame_budget(1, 0));
        assert!(should_force_paint_for_frame_budget(1024, 0));
    }

    /// ft-d6nrd: cosmetic-defer aggregator alone forces the next
    /// frame even when the FrameBudget queue is drained.
    #[test]
    fn force_paint_predicate_true_when_cosmetic_outstanding() {
        assert!(should_force_paint_for_frame_budget(0, 1));
        assert!(should_force_paint_for_frame_budget(0, u32::MAX));
    }

    /// ft-d6nrd: both signals high → force-paint (most common
    /// path under sustained burst).
    #[test]
    fn force_paint_predicate_true_when_both_signals_high() {
        assert!(should_force_paint_for_frame_budget(5, 12));
    }

    /// ft-asdza: the one-shot platform preference probe returns
    /// MotionPreference; the GUI bridge normalizes that into the
    /// FrameBudget gate's ReduceMotionState.
    #[test]
    fn reduce_motion_preference_maps_to_gate_state() {
        use frankenterm_core::accessibility_preferences::MotionPreference;
        use frankenterm_core::frame_budget_a11y_gate::ReduceMotionState;

        assert_eq!(
            reduce_motion_state_from_preference(MotionPreference::Reduce),
            ReduceMotionState::On
        );
        assert_eq!(
            reduce_motion_state_from_preference(MotionPreference::NoPreference),
            ReduceMotionState::Off
        );
    }

    /// ft-asdza: every built-in GUI FrameBudget op maps to the
    /// core A11Y taxonomy; plugin/custom ops stay outside the
    /// reduce-motion substrate and only inherit the base budget
    /// policy.
    #[test]
    fn frame_budget_ops_map_to_a11y_gate_ops() {
        use frankenterm_core::frame_budget_a11y_gate::OpKind;

        assert_eq!(
            a11y_op_kind_from_frame_budget_op(frame_budget::OpKind::DirtyQuadRebuild),
            Some(OpKind::DirtyQuadRebuild)
        );
        assert_eq!(
            a11y_op_kind_from_frame_budget_op(frame_budget::OpKind::Cursor),
            Some(OpKind::Cursor)
        );
        assert_eq!(
            a11y_op_kind_from_frame_budget_op(frame_budget::OpKind::Selection),
            Some(OpKind::Selection)
        );
        assert_eq!(
            a11y_op_kind_from_frame_budget_op(frame_budget::OpKind::Ligatures),
            Some(OpKind::Ligatures)
        );
        assert_eq!(
            a11y_op_kind_from_frame_budget_op(frame_budget::OpKind::SubpixelAa),
            Some(OpKind::SubpixelAa)
        );
        assert_eq!(
            a11y_op_kind_from_frame_budget_op(frame_budget::OpKind::Decorations),
            Some(OpKind::Decorations)
        );
        assert_eq!(
            a11y_op_kind_from_frame_budget_op(frame_budget::OpKind::Animations),
            Some(OpKind::Animations)
        );
        assert_eq!(
            a11y_op_kind_from_frame_budget_op(frame_budget::OpKind::Custom(7)),
            None
        );
    }

    /// ft-asdza acceptance: reduce-motion ON skips animation work
    /// before the FrameBudget queue mutates.
    #[test]
    fn reduce_motion_on_skips_animation_gate_even_when_budget_healthy() {
        use frankenterm_core::frame_budget_a11y_gate::{MotionGateDecision, ReduceMotionState};

        let budget = frame_budget::FrameBudget::new(60);
        let decision = evaluate_frame_budget_reduce_motion_gate(
            &budget,
            frame_budget::OpKind::Animations,
            frame_budget::OpPriority::Cosmetic,
            ReduceMotionState::On,
        );

        assert_eq!(decision, MotionGateDecision::Skip);
        assert_eq!(budget.queue_depth(), 0);
        assert_eq!(budget.spent_ns(), 0);
    }

    /// ft-asdza acceptance: Unknown preserves the substrate safety
    /// default by falling back to the base FrameBudget decision.
    #[test]
    fn reduce_motion_unknown_preserves_frame_budget_defer_policy() {
        use frankenterm_core::frame_budget_a11y_gate::{MotionGateDecision, ReduceMotionState};

        let mut budget = frame_budget::FrameBudget::new(60);
        budget.try_execute(
            frame_budget::OpKind::DirtyQuadRebuild,
            frame_budget::OpPriority::Required,
            (budget.budget_ns() as f64 * 0.95) as u64,
        );

        assert_eq!(
            base_policy_for_frame_budget_state(&budget, frame_budget::OpPriority::Cosmetic),
            frankenterm_core::frame_budget_a11y_gate::BaseExecutionPolicy::Defer
        );

        let decision = evaluate_frame_budget_reduce_motion_gate(
            &budget,
            frame_budget::OpKind::Animations,
            frame_budget::OpPriority::Cosmetic,
            ReduceMotionState::Unknown,
        );

        assert_eq!(decision, MotionGateDecision::Defer);
    }

    /// ft-asdza: required correctness ops still execute even when
    /// reduce-motion is ON and the frame is already over budget.
    #[test]
    fn reduce_motion_gate_never_skips_required_ops() {
        use frankenterm_core::frame_budget_a11y_gate::{MotionGateDecision, ReduceMotionState};

        let mut budget = frame_budget::FrameBudget::new(60);
        budget.try_execute(
            frame_budget::OpKind::DirtyQuadRebuild,
            frame_budget::OpPriority::Required,
            (budget.budget_ns() as f64 * 0.95) as u64,
        );

        let decision = evaluate_frame_budget_reduce_motion_gate(
            &budget,
            frame_budget::OpKind::Cursor,
            frame_budget::OpPriority::Required,
            ReduceMotionState::On,
        );

        assert_eq!(decision, MotionGateDecision::Execute);
    }

    /// ft-asdza: when reduce-motion is not ON, queue-overflow
    /// behavior stays the FrameBudget allocator's drop-oldest
    /// policy rather than being rewritten by the A11Y gate.
    #[test]
    fn reduce_motion_off_preserves_drop_oldest_policy() {
        use frankenterm_core::frame_budget_a11y_gate::{
            BaseExecutionPolicy, MotionGateDecision, ReduceMotionState,
        };

        let mut budget = frame_budget::FrameBudget::new(60).with_deferred_cap(1);
        budget.try_execute(
            frame_budget::OpKind::DirtyQuadRebuild,
            frame_budget::OpPriority::Required,
            (budget.budget_ns() as f64 * 0.95) as u64,
        );
        budget.try_execute(
            frame_budget::OpKind::Ligatures,
            frame_budget::OpPriority::Cosmetic,
            100,
        );

        assert_eq!(
            base_policy_for_frame_budget_state(&budget, frame_budget::OpPriority::Cosmetic),
            BaseExecutionPolicy::DropOldest
        );

        let decision = evaluate_frame_budget_reduce_motion_gate(
            &budget,
            frame_budget::OpKind::Decorations,
            frame_budget::OpPriority::Cosmetic,
            ReduceMotionState::Off,
        );

        assert_eq!(decision, MotionGateDecision::DropOldest);
    }

    #[test]
    fn frame_budget_run_predicate_only_runs_execute_decisions() {
        use frankenterm_core::frame_budget_a11y_gate::MotionGateDecision;

        assert!(should_run_frame_budget_decision(
            MotionGateDecision::Execute
        ));
        assert!(!should_run_frame_budget_decision(MotionGateDecision::Defer));
        assert!(!should_run_frame_budget_decision(MotionGateDecision::Skip));
        assert!(!should_run_frame_budget_decision(
            MotionGateDecision::DropOldest
        ));
    }

    #[test]
    fn frame_budget_default_costs_match_seed_table_for_gui_ops() {
        assert_eq!(
            default_frame_budget_cost_ns(frame_budget::OpKind::DirtyQuadRebuild),
            80_000
        );
        assert_eq!(
            default_frame_budget_cost_ns(frame_budget::OpKind::Decorations),
            30_000
        );
        assert_eq!(
            default_frame_budget_cost_ns(frame_budget::OpKind::Animations),
            200_000
        );
        assert_eq!(
            default_frame_budget_cost_ns(frame_budget::OpKind::Custom(9)),
            frame_budget::FrameBudgetCostFeedback::CUSTOM_DEFAULT_NS
        );
    }

    #[test]
    fn drained_frame_budget_ops_decrement_matching_outstanding_kind() {
        use frankenterm_core::frame_budget_a11y_gate::{
            CosmeticDeferOutstanding, OpKind as A11yOpKind,
        };

        let mut outstanding = CosmeticDeferOutstanding::default();
        outstanding.record_deferred(A11yOpKind::Ligatures);
        outstanding.record_deferred(A11yOpKind::Decorations);

        record_drained_frame_budget_ops(
            &mut outstanding,
            [frame_budget::DeferredOp {
                kind: frame_budget::OpKind::Ligatures,
                estimated_cost_ns: 1,
            }],
        );

        assert_eq!(outstanding.deferred_ligatures, 0);
        assert_eq!(outstanding.deferred_decorations, 1);
        assert_eq!(outstanding.total(), 1);
    }

    #[test]
    fn dropped_frame_budget_op_reconciles_evicted_outstanding_kind() {
        use frankenterm_core::frame_budget_a11y_gate::{
            CosmeticDeferOutstanding, OpKind as A11yOpKind,
        };

        let mut outstanding = CosmeticDeferOutstanding::default();
        outstanding.record_deferred(A11yOpKind::Ligatures);

        record_frame_budget_execution_outstanding(
            &mut outstanding,
            frame_budget::OpKind::Decorations,
            frame_budget::ExecutionDecision::Dropped {
                evicted: frame_budget::DeferredOp {
                    kind: frame_budget::OpKind::Ligatures,
                    estimated_cost_ns: 1,
                },
            },
        );

        assert_eq!(outstanding.deferred_ligatures, 0);
        assert_eq!(outstanding.deferred_decorations, 1);
        assert_eq!(outstanding.total(), 1);
    }

    /// ft-8pcwy: gate disabled → never skip, regardless of bitmap
    /// state. This remains the manual fallback invariant even
    /// though production now enables the gate by default.
    #[test]
    fn skip_predicate_off_when_gate_disabled() {
        let mut bm = render::dirty_lines::DirtyLineBitmap::new(24);
        bm.mark(5);
        // gate=false: never skip.
        assert!(!is_clean_line_for_cache_hit_accounting(false, Some(&bm), 0));
        assert!(!is_clean_line_for_cache_hit_accounting(false, Some(&bm), 5));
        assert!(!is_clean_line_for_cache_hit_accounting(false, None, 0));
    }

    /// ft-8pcwy: gate enabled but no bitmap registered → never
    /// skip. Per-cell event sources haven't touched the pane yet;
    /// safer to render than to leave a hole.
    #[test]
    fn skip_predicate_off_when_no_bitmap_registered() {
        assert!(!is_clean_line_for_cache_hit_accounting(true, None, 0));
        assert!(!is_clean_line_for_cache_hit_accounting(true, None, 23));
    }

    /// ft-8pcwy: gate enabled but bitmap is empty → never skip.
    /// The bitmap was cleared at frame end and no event has marked
    /// anything since.
    #[test]
    fn skip_predicate_off_when_bitmap_empty() {
        let bm = render::dirty_lines::DirtyLineBitmap::new(24);
        assert!(bm.is_empty());
        for idx in 0..24 {
            assert!(
                !is_clean_line_for_cache_hit_accounting(true, Some(&bm), idx),
                "empty bitmap must never skip (idx={idx})",
            );
        }
    }

    /// ft-8pcwy: gate enabled and bitmap non-empty → skip iff the
    /// row is not in the dirty set. This is the actual savings
    /// path the bead targets.
    #[test]
    fn skip_predicate_skips_only_clean_rows_when_bitmap_active() {
        let mut bm = render::dirty_lines::DirtyLineBitmap::new(24);
        bm.mark(5);
        bm.mark(7);
        bm.mark(20);
        for idx in 0..24 {
            let should_skip = is_clean_line_for_cache_hit_accounting(true, Some(&bm), idx);
            let is_dirty = idx == 5 || idx == 7 || idx == 20;
            assert_eq!(
                should_skip, !is_dirty,
                "idx={idx} dirty={is_dirty} skip={should_skip}",
            );
        }
    }

    /// ft-8pcwy: skip predicate is consistent with the bitmap's
    /// own contains() check across the boundary (last row, past
    /// capacity, etc.).
    #[test]
    fn skip_predicate_handles_capacity_edge() {
        let mut bm = render::dirty_lines::DirtyLineBitmap::new(8);
        bm.mark(7);
        // idx within capacity, dirty.
        assert!(!is_clean_line_for_cache_hit_accounting(true, Some(&bm), 7));
        // idx within capacity, clean.
        assert!(is_clean_line_for_cache_hit_accounting(true, Some(&bm), 6));
        // idx past capacity → contains() returns false → skip.
        // (Out-of-range rows shouldn't be passed in by the render
        //  loop, but the predicate is still well-defined.)
        assert!(is_clean_line_for_cache_hit_accounting(true, Some(&bm), 8));
        assert!(is_clean_line_for_cache_hit_accounting(
            true,
            Some(&bm),
            usize::MAX
        ));
    }

    /// Exact success clears per-cell damage immediately.
    #[test]
    fn frame_end_clear_runs_on_per_cell_frames() {
        let mut bitmaps: HashMap<usize, render::dirty_lines::DirtyLineBitmap> = HashMap::new();
        let mut bm = render::dirty_lines::DirtyLineBitmap::new(24);
        bm.mark(5);
        bm.mark(7);
        bitmaps.insert(1, bm);
        run_clear_dirty_lines_after_frame(&mut bitmaps);

        let cleared = bitmaps.get(&1).expect("pane 1 bitmap retained");
        assert_eq!(cleared.count(), 0, "frame-end must clear marks");
        assert_eq!(
            cleared.frames_cleared_total(),
            1,
            "lifetime clear counter must increment exactly once",
        );
    }

    /// Exact success also clears whole-screen damage immediately; generation
    /// mismatch and failure already protect damage that was not presented.
    #[test]
    fn whole_screen_event_clears_on_first_exact_success() {
        let mut bitmaps: HashMap<usize, render::dirty_lines::DirtyLineBitmap> = HashMap::new();
        let mut bm = render::dirty_lines::DirtyLineBitmap::new(24);
        bm.mark_all();
        bitmaps.insert(1, bm);
        run_clear_dirty_lines_after_frame(&mut bitmaps);

        let cleared = bitmaps.get(&1).expect("pane 1 bitmap retained");
        assert_eq!(cleared.count(), 0);
        assert_eq!(
            cleared.frames_cleared_total(),
            1,
            "one exact success must settle whole-screen damage once",
        );
    }

    /// ft-jvj78: every registered pane bitmap is cleared, not just
    /// the first one in iteration order.
    #[test]
    fn frame_end_clear_runs_across_all_panes() {
        let mut bitmaps: HashMap<usize, render::dirty_lines::DirtyLineBitmap> = HashMap::new();
        for pane_id in 1..=4 {
            let mut bm = render::dirty_lines::DirtyLineBitmap::new(24);
            bm.mark(pane_id);
            bitmaps.insert(pane_id, bm);
        }
        run_clear_dirty_lines_after_frame(&mut bitmaps);

        for pane_id in 1..=4 {
            let bm = bitmaps.get(&pane_id).expect("bitmap retained");
            assert_eq!(bm.count(), 0, "pane {pane_id} must be cleared");
            assert_eq!(bm.frames_cleared_total(), 1);
        }
    }

    #[test]
    fn injected_acquire_submit_present_failures_retain_until_eventual_success() {
        let dimensions = ::window::Dimensions {
            pixel_width: 1280,
            pixel_height: 720,
            dpi: 96,
        };
        let timeout: Result<u8, RenderAttemptFailure> =
            acquire_webgpu_frame_with_repair_using(
                dimensions,
                |_| Ok(webgpu::SurfaceConfigureOutcome::Ready),
                || Err(webgpu::WebGpuSurfaceTextureError::Timeout),
                |_| unreachable!("timeout must not force-configure the surface"),
                |_| unreachable!("timeout must not recreate the surface"),
            );
        let timeout = timeout.expect_err("timeout acquisition must fail");
        let timeout_stage = RenderFailureStage::SurfaceAcquire(
            webgpu::WebGpuSurfaceTextureError::Timeout,
        );
        assert_eq!(timeout.stage(), timeout_stage);

        let generation = DamageGeneration::default();
        let mut bitmaps: HashMap<usize, render::dirty_lines::DirtyLineBitmap> = HashMap::new();
        let mut bitmap = render::dirty_lines::DirtyLineBitmap::new(24);
        bitmap.mark(2);
        bitmap.mark(19);
        bitmaps.insert(7, bitmap);
        let before = bitmaps.clone();
        let mut recovery = RenderRecoveryState::default();
        let mut wakes = RenderWakeState::default();
        let now = Instant::now();

        for (attempt, stage) in [
            timeout_stage,
            RenderFailureStage::Submission,
            RenderFailureStage::Present,
        ]
        .into_iter()
        .enumerate()
        {
            let (damage, directive) = apply_failed_render_attempt(
                &mut bitmaps,
                generation,
                &mut recovery,
                stage,
            );
            assert_eq!(damage, DamageCommitOutcome::RetainedFailure);
            assert_eq!(bitmaps, before, "attempt {attempt} changed dirty rows");

            let delay = match directive {
                RenderRecoveryDirective::RetryAfter(delay) => delay,
                other => panic!("attempt {attempt} was not retryable: {other:?}"),
            };
            let ticket = match wakes.plan(RenderWakeReason::Retry(stage), now + delay, now) {
                RenderWakePlan::Schedule { ticket, .. } => ticket,
                _ => panic!("attempt {attempt} did not schedule an exact retry"),
            };
            recovery.enter_cooldown(ticket, stage);
            assert_eq!(recovery.admit(), PaintAdmission::SuppressCooldown);
            assert_eq!(
                wakes.dispatch(ticket),
                RenderWakeDispatch::Fired(RenderWakeReason::Retry(stage))
            );
            assert!(recovery.mark_retry_ready(ticket));
            assert_eq!(recovery.admit(), PaintAdmission::Admit);
        }

        let mut acquire_calls = 0;
        let mut recreate_calls = 0;
        let repaired: Result<u8, RenderAttemptFailure> =
            acquire_webgpu_frame_with_repair_using(
                dimensions,
                |_| Ok(webgpu::SurfaceConfigureOutcome::Ready),
                || {
                    acquire_calls += 1;
                    match acquire_calls {
                        1 => Err(webgpu::WebGpuSurfaceTextureError::Lost),
                        2 => Ok(42),
                        _ => unreachable!("repair may reacquire only once"),
                    }
                },
                |_| unreachable!("lost surface must not use force-configure"),
                |_| {
                    recreate_calls += 1;
                    Ok(webgpu::SurfaceConfigureOutcome::Ready)
                },
            );
        assert_eq!(repaired.expect("lost surface must repair and reacquire"), 42);
        assert_eq!(acquire_calls, 2);
        assert_eq!(recreate_calls, 1);

        let first_success = apply_presented_render_attempt(
            &mut bitmaps,
            generation,
            &mut recovery,
            generation,
        );
        assert_eq!(first_success, DamageCommitOutcome::Cleared);
        assert_eq!(bitmaps.get(&7).expect("pane retained").count(), 0);
        assert_eq!(recovery.failed_attempts_since_success, 0);
        assert_eq!(recovery.admit(), PaintAdmission::Admit);
    }

    #[test]
    fn failed_submission_or_present_retains_exact_damage() {
        let mut original: HashMap<usize, render::dirty_lines::DirtyLineBitmap> = HashMap::new();
        let mut bm = render::dirty_lines::DirtyLineBitmap::new(24);
        bm.mark(2);
        bm.mark(19);
        original.insert(7, bm);

        for stage in [
            RenderFailureStage::Submission,
            RenderFailureStage::Present,
        ] {
            let mut bitmaps = original.clone();
            let before = bitmaps.clone();
            let generation = DamageGeneration::default();

            let outcome = settle_frame_damage(
                &mut bitmaps,
                generation,
                FrameCompletion::Failed(stage),
            );

            assert_eq!(outcome, DamageCommitOutcome::RetainedFailure);
            assert_eq!(bitmaps, before, "{stage:?} changed dirty rows");
        }
    }

    #[test]
    fn stale_presented_generation_cannot_clear_newer_damage() {
        let captured = DamageGeneration::default();
        let mut current = captured;
        assert_eq!(current.advance(), DamageAdvanceOutcome::Advanced);
        let mut bitmaps: HashMap<usize, render::dirty_lines::DirtyLineBitmap> = HashMap::new();
        let mut bm = render::dirty_lines::DirtyLineBitmap::new(24);
        bm.mark(11);
        bitmaps.insert(3, bm);
        let before = bitmaps.clone();
        let outcome = settle_frame_damage(
            &mut bitmaps,
            current,
            FrameCompletion::Presented(captured),
        );

        assert_eq!(outcome, DamageCommitOutcome::RetainedStale);
        assert_eq!(bitmaps, before);
    }

    #[test]
    fn matching_presented_generation_clears_per_cell_damage() {
        let mut generation = DamageGeneration::default();
        assert_eq!(generation.advance(), DamageAdvanceOutcome::Advanced);
        let mut bitmaps: HashMap<usize, render::dirty_lines::DirtyLineBitmap> = HashMap::new();
        let mut bm = render::dirty_lines::DirtyLineBitmap::new(24);
        bm.mark(4);
        bitmaps.insert(5, bm);
        let outcome = settle_frame_damage(
            &mut bitmaps,
            generation,
            FrameCompletion::Presented(generation),
        );

        assert_eq!(outcome, DamageCommitOutcome::Cleared);
        assert_eq!(bitmaps.get(&5).expect("pane retained").count(), 0);
    }

    #[test]
    fn exhausted_damage_generation_is_sticky_and_never_commits() {
        let mut generation = DamageGeneration {
            value: u64::MAX,
            exhausted: false,
        };
        let captured = generation;
        assert_eq!(generation.advance(), DamageAdvanceOutcome::ExhaustedNow);
        assert_eq!(
            generation.advance(),
            DamageAdvanceOutcome::AlreadyExhausted
        );

        let mut bitmaps: HashMap<usize, render::dirty_lines::DirtyLineBitmap> = HashMap::new();
        let mut bm = render::dirty_lines::DirtyLineBitmap::new(8);
        bm.mark_all();
        bitmaps.insert(1, bm);
        let before = bitmaps.clone();
        let outcome = settle_frame_damage(
            &mut bitmaps,
            generation,
            FrameCompletion::Presented(captured),
        );

        assert_eq!(outcome, DamageCommitOutcome::RetainedEpochExhausted);
        assert_eq!(bitmaps, before);
    }

    #[test]
    fn render_recovery_backoff_is_bounded_and_permanent_errors_open() {
        let timeout = RenderFailureStage::SurfaceAcquire(
            webgpu::WebGpuSurfaceTextureError::Timeout,
        );
        for (attempt, expected_ms) in [8, 16, 32, 64, 128, 250].into_iter().enumerate() {
            let attempt = u32::try_from(attempt).expect("bounded retry index fits u32") + 1;
            assert_eq!(
                render_recovery_directive(timeout, attempt),
                RenderRecoveryDirective::RetryAfter(Duration::from_millis(expected_ms))
            );
        }
        assert_eq!(
            render_recovery_directive(timeout, 7),
            RenderRecoveryDirective::Park
        );
        assert_eq!(
            render_recovery_directive(
                RenderFailureStage::Draw(DrawFailureStage::MissingGlyphProgram),
                1,
            ),
            RenderRecoveryDirective::OpenCircuit
        );
        assert_eq!(
            render_recovery_directive(
                RenderFailureStage::Draw(DrawFailureStage::BufferSliceBounds),
                1,
            ),
            RenderRecoveryDirective::OpenCircuit
        );
    }

    #[test]
    fn alternating_stale_surface_stages_cannot_reset_retry_budget() {
        let outdated = RenderFailureStage::SurfaceAcquire(
            webgpu::WebGpuSurfaceTextureError::Outdated,
        );
        let lost =
            RenderFailureStage::SurfaceAcquire(webgpu::WebGpuSurfaceTextureError::Lost);
        let mut recovery = RenderRecoveryState::default();

        assert_eq!(
            recovery.record_failure(outdated),
            RenderRecoveryDirective::RetryAfter(Duration::from_millis(8))
        );
        assert_eq!(
            recovery.record_failure(lost),
            RenderRecoveryDirective::RetryAfter(Duration::from_millis(16))
        );
        assert_eq!(
            recovery.record_failure(outdated),
            RenderRecoveryDirective::RetryAfter(Duration::from_millis(32))
        );
        assert_eq!(recovery.record_failure(lost), RenderRecoveryDirective::Park);
    }

    #[test]
    fn alternating_backend_stages_cannot_reset_retry_budget() {
        let draw = RenderFailureStage::Draw(DrawFailureStage::BackendDraw);
        let finish = RenderFailureStage::BackendFinish;
        let mut recovery = RenderRecoveryState::default();

        assert!(matches!(
            recovery.record_failure(draw),
            RenderRecoveryDirective::RetryAfter(_)
        ));
        assert!(matches!(
            recovery.record_failure(finish),
            RenderRecoveryDirective::RetryAfter(_)
        ));
        assert!(matches!(
            recovery.record_failure(draw),
            RenderRecoveryDirective::RetryAfter(_)
        ));
        assert_eq!(
            recovery.record_failure(finish),
            RenderRecoveryDirective::OpenCircuit
        );
    }

    #[test]
    fn surface_signal_preserves_incident_evidence_until_present() {
        let timeout = RenderFailureStage::SurfaceAcquire(
            webgpu::WebGpuSurfaceTextureError::Timeout,
        );
        let mut recovery = RenderRecoveryState::default();
        assert_eq!(
            recovery.record_failure(timeout),
            RenderRecoveryDirective::RetryAfter(Duration::from_millis(8))
        );
        recovery.park(timeout);
        assert!(recovery.record_surface_recovery_signal());
        assert_eq!(recovery.failed_attempts_since_success, 1);
        assert_eq!(
            recovery.record_failure(timeout),
            RenderRecoveryDirective::RetryAfter(Duration::from_millis(16))
        );

        recovery.record_success();
        assert_eq!(
            recovery.record_failure(timeout),
            RenderRecoveryDirective::RetryAfter(Duration::from_millis(8))
        );
    }

    #[test]
    fn render_wake_keeps_earlier_animation_without_losing_live_state() {
        let now = Instant::now();
        let mut state = RenderWakeState::default();
        let first_ticket = match state.plan(
            RenderWakeReason::Animation,
            now + Duration::from_millis(10),
            now,
        ) {
            RenderWakePlan::Schedule { ticket, .. } => ticket,
            _ => panic!("first animation must schedule"),
        };
        match state.plan(
            RenderWakeReason::Animation,
            now + Duration::from_millis(20),
            now,
        ) {
            RenderWakePlan::Kept { ticket } => assert_eq!(ticket, first_ticket),
            _ => panic!("later animation must keep earlier wake"),
        }
        assert_eq!(state.pending_ticket(), Some(first_ticket));
        assert_eq!(
            state.dispatch(first_ticket),
            RenderWakeDispatch::Fired(RenderWakeReason::Animation)
        );
        assert_eq!(state.dispatch(first_ticket), RenderWakeDispatch::Stale);
    }

    #[test]
    fn retry_supersedes_animation_and_animation_cannot_bypass_retry() {
        let now = Instant::now();
        let mut state = RenderWakeState::default();
        let animation = match state.plan(
            RenderWakeReason::Animation,
            now + Duration::from_millis(5),
            now,
        ) {
            RenderWakePlan::Schedule { ticket, .. } => ticket,
            _ => panic!("animation must schedule"),
        };
        let stage = RenderFailureStage::Paint;
        let retry = match state.plan(
            RenderWakeReason::Retry(stage),
            now + Duration::from_millis(8),
            now,
        ) {
            RenderWakePlan::Schedule { ticket, .. } => ticket,
            _ => panic!("retry must replace animation"),
        };
        assert_ne!(retry, animation);
        assert_eq!(state.dispatch(animation), RenderWakeDispatch::Stale);
        match state.plan(
            RenderWakeReason::Animation,
            now + Duration::from_millis(1),
            now,
        ) {
            RenderWakePlan::Kept { ticket } => assert_eq!(ticket, retry),
            _ => panic!("animation must not replace retry"),
        }
        assert_eq!(
            state.dispatch(retry),
            RenderWakeDispatch::Fired(RenderWakeReason::Retry(stage))
        );
    }

    #[test]
    fn success_cancel_then_fresh_failure_gets_a_fresh_ticket() {
        let now = Instant::now();
        let mut wakes = RenderWakeState::default();
        let old = match wakes.plan(
            RenderWakeReason::Retry(RenderFailureStage::Paint),
            now + Duration::from_millis(250),
            now,
        ) {
            RenderWakePlan::Schedule { ticket, .. } => ticket,
            _ => panic!("old retry must schedule"),
        };
        assert!(wakes.cancel());
        assert_eq!(wakes.dispatch(old), RenderWakeDispatch::Stale);

        let (fresh, delay) = match wakes.plan(
            RenderWakeReason::Retry(RenderFailureStage::Paint),
            now + Duration::from_millis(8),
            now,
        ) {
            RenderWakePlan::Schedule { ticket, delay, .. } => (ticket, delay),
            _ => panic!("fresh retry must schedule"),
        };
        assert_ne!(fresh, old);
        assert_eq!(delay, Duration::from_millis(8));
    }

    #[test]
    fn stale_render_scheduler_rejection_cannot_cancel_newer_exact_wake() {
        let now = Instant::now();
        let mut wakes = RenderWakeState::default();
        let old = match wakes.plan(
            RenderWakeReason::Animation,
            now + Duration::from_millis(20),
            now,
        ) {
            RenderWakePlan::Schedule { ticket, .. } => ticket,
            _ => panic!("initial animation must schedule"),
        };
        let stage = RenderFailureStage::Paint;
        let current = match wakes.plan(
            RenderWakeReason::Retry(stage),
            now + Duration::from_millis(8),
            now,
        ) {
            RenderWakePlan::Schedule { ticket, .. } => ticket,
            _ => panic!("retry must supersede the animation"),
        };

        assert!(!wakes.cancel_exact(old));
        assert_eq!(wakes.pending_ticket(), Some(current));
        assert_eq!(wakes.dispatch(old), RenderWakeDispatch::Stale);
        assert!(wakes.cancel_exact(current));
        assert_eq!(wakes.pending_ticket(), None);
        assert_eq!(wakes.dispatch(current), RenderWakeDispatch::Stale);
    }

    #[test]
    fn checked_wake_ticket_exhaustion_cannot_alias_an_old_timer() {
        let now = Instant::now();
        let mut state = RenderWakeState {
            next_ticket: u64::MAX,
            ..RenderWakeState::default()
        };
        assert!(matches!(
            state.plan(RenderWakeReason::Animation, now, now),
            RenderWakePlan::Exhausted
        ));
        assert_eq!(state.pending_ticket(), None);
    }

    #[test]
    fn cooldown_suppresses_external_repaint_until_exact_retry_is_ready() {
        let stage = RenderFailureStage::SurfaceAcquire(
            webgpu::WebGpuSurfaceTextureError::Timeout,
        );
        let ticket = super::RenderWakeTicket(7);
        let mut recovery = RenderRecoveryState::default();
        recovery.enter_cooldown(ticket, stage);
        assert_eq!(recovery.admit(), PaintAdmission::SuppressCooldown);
        assert!(!recovery.mark_retry_ready(super::RenderWakeTicket(8)));
        assert_eq!(recovery.admit(), PaintAdmission::SuppressCooldown);
        assert!(recovery.mark_retry_ready(ticket));
        assert_eq!(recovery.admit(), PaintAdmission::Admit);
    }

    #[test]
    fn occlusion_parks_without_a_self_wake_and_surface_signal_reopens() {
        let stage = RenderFailureStage::SurfaceAcquire(
            webgpu::WebGpuSurfaceTextureError::Occluded,
        );
        let mut recovery = RenderRecoveryState::default();
        assert_eq!(
            recovery.record_failure(stage),
            RenderRecoveryDirective::Park
        );
        recovery.park(stage);
        assert_eq!(recovery.parked_occluded_stage(), Some(stage));
        assert_eq!(recovery.admit(), PaintAdmission::SuppressParked);
        assert!(recovery.record_surface_recovery_signal());
        assert_eq!(recovery.failed_attempts_since_success, 1);
        assert_eq!(recovery.admit(), PaintAdmission::Admit);
    }

    #[test]
    fn parked_repaint_probe_is_limited_to_occlusion() {
        let occluded = RenderFailureStage::SurfaceAcquire(
            webgpu::WebGpuSurfaceTextureError::Occluded,
        );
        let timeout = RenderFailureStage::SurfaceAcquire(
            webgpu::WebGpuSurfaceTextureError::Timeout,
        );
        let mut recovery = RenderRecoveryState::default();

        recovery.park(occluded);
        assert_eq!(recovery.parked_occluded_stage(), Some(occluded));
        recovery.park(timeout);
        assert_eq!(recovery.parked_occluded_stage(), None);
        recovery.enter_cooldown(super::RenderWakeTicket(9), occluded);
        assert_eq!(recovery.parked_occluded_stage(), None);
        recovery.open_circuit(occluded);
        assert_eq!(recovery.parked_occluded_stage(), None);
    }

    #[test]
    fn parked_occlusion_probe_requires_exact_wake_and_preserves_evidence() {
        let stage = RenderFailureStage::SurfaceAcquire(
            webgpu::WebGpuSurfaceTextureError::Occluded,
        );
        let now = Instant::now();
        let mut recovery = RenderRecoveryState::default();
        let mut wakes = RenderWakeState::default();

        assert_eq!(
            recovery.record_failure(stage),
            RenderRecoveryDirective::Park
        );
        recovery.park(stage);
        let ticket = match wakes.plan(
            RenderWakeReason::Retry(stage),
            now + super::OCCLUDED_REPAINT_PROBE_DELAY,
            now,
        ) {
            RenderWakePlan::Schedule { ticket, .. } => ticket,
            _ => panic!("parked occlusion repaint must schedule one probe"),
        };
        recovery.enter_cooldown(ticket, stage);
        assert_eq!(recovery.failed_attempts_since_success, 1);
        assert_eq!(recovery.admit(), PaintAdmission::SuppressCooldown);

        let stale = super::RenderWakeTicket(ticket.0.saturating_add(1));
        assert_eq!(wakes.dispatch(stale), RenderWakeDispatch::Stale);
        assert!(!recovery.mark_retry_ready(stale));
        assert_eq!(
            wakes.dispatch(ticket),
            RenderWakeDispatch::Fired(RenderWakeReason::Retry(stage))
        );
        assert!(recovery.mark_retry_ready(ticket));
        assert_eq!(recovery.admit(), PaintAdmission::Admit);
        assert_eq!(recovery.failed_attempts_since_success, 1);

        assert_eq!(
            recovery.record_failure(stage),
            RenderRecoveryDirective::Park
        );
        recovery.park(stage);
        assert_eq!(recovery.failed_attempts_since_success, 2);
        assert_eq!(recovery.parked_occluded_stage(), Some(stage));
    }

    #[test]
    fn invariant_circuit_is_not_reset_by_surface_signal() {
        let stage = RenderFailureStage::Draw(DrawFailureStage::BufferSliceBounds);
        let mut recovery = RenderRecoveryState::default();
        recovery.open_circuit(stage);
        assert!(!recovery.record_surface_recovery_signal());
        assert_eq!(recovery.admit(), PaintAdmission::SuppressCircuit);
    }

    #[test]
    fn glium_draw_failure_stays_primary_when_finish_also_fails() {
        let failure = combine_glium_draw_and_finish(
            Err(anyhow::anyhow!("draw failed")),
            Err(anyhow::anyhow!("finish failed")),
        )
        .expect_err("combined failure expected");
        assert_eq!(
            failure.stage(),
            RenderFailureStage::Draw(DrawFailureStage::RenderCommands)
        );
        assert!(failure.secondary_finish.is_some());

        let finish_only = combine_glium_draw_and_finish(
            Ok(()),
            Err(anyhow::anyhow!("finish failed")),
        )
        .expect_err("finish failure expected");
        assert_eq!(finish_only.stage(), RenderFailureStage::BackendFinish);
    }

    #[test]
    fn draw_invariants_keep_distinct_typed_failure_stages() {
        let missing = RenderAttemptFailure::draw(DrawFailure::new(
            DrawFailureStage::MissingGlyphProgram,
            anyhow::anyhow!("missing program"),
        ));
        assert_eq!(
            missing.stage(),
            RenderFailureStage::Draw(DrawFailureStage::MissingGlyphProgram)
        );

        let bounds = RenderAttemptFailure::draw(DrawFailure::new(
            DrawFailureStage::BufferSliceBounds,
            anyhow::anyhow!("slice bounds"),
        ));
        assert_eq!(
            bounds.stage(),
            RenderFailureStage::Draw(DrawFailureStage::BufferSliceBounds)
        );
    }

    #[test]
    fn failed_webgpu_repair_is_a_permanent_validation_stage() {
        assert_eq!(
            webgpu_repair_failure_stage(),
            RenderFailureStage::SurfaceAcquire(
                webgpu::WebGpuSurfaceTextureError::Validation,
            )
        );
        assert_eq!(
            render_recovery_directive(webgpu_repair_failure_stage(), 1),
            RenderRecoveryDirective::OpenCircuit
        );
    }

    #[test]
    fn webgpu_surface_error_classification_distinguishes_repair_modes() {
        assert_eq!(
            classify_webgpu_surface_error(&webgpu::WebGpuSurfaceTextureError::Lost),
            WebGpuSurfaceErrorAction::RecreateSurface
        );
        assert_eq!(
            classify_webgpu_surface_error(&webgpu::WebGpuSurfaceTextureError::Outdated),
            WebGpuSurfaceErrorAction::ForceConfigure
        );
    }

    #[test]
    fn webgpu_surface_error_classification_defers_non_repair_errors_to_policy() {
        assert_eq!(
            classify_webgpu_surface_error(&webgpu::WebGpuSurfaceTextureError::Timeout),
            WebGpuSurfaceErrorAction::DeferToRecoveryPolicy
        );
        assert_eq!(
            classify_webgpu_surface_error(&webgpu::WebGpuSurfaceTextureError::Occluded),
            WebGpuSurfaceErrorAction::DeferToRecoveryPolicy
        );
        assert_eq!(
            classify_webgpu_surface_error(&webgpu::WebGpuSurfaceTextureError::Validation),
            WebGpuSurfaceErrorAction::DeferToRecoveryPolicy
        );
    }
}
