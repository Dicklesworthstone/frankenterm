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
use crate::termwindow::background::{
    LoadedBackgroundLayer, load_background_image, reload_background_image,
};
use crate::termwindow::keyevent::{KeyTableArgs, KeyTableState};
use crate::termwindow::modal::Modal;
use crate::termwindow::render::paint::AllowImage;
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
    GuiFloatingPaneController, emit_floating_pane_a11y_messages,
};
use frankenterm_gui::triple_buffer_gui::TerminalStateTripleBufferRegistry;
use lfucache::*;
use mlua::{FromLua, LuaSerdeExt, UserData, UserDataFields};
use mux::pane::{
    CachePolicy, CloseReason, Pane, PaneId, Pattern as MuxPattern, PerformAssignmentResult,
};
use mux::renderable::{RenderableDimensions, StableCursorPosition};
use mux::tab::{
    PositionedPane, PositionedSplit, SplitDirection, SplitRequest, SplitSize as MuxSplitSize, Tab,
    TabId,
};
use mux::window::WindowId as MuxWindowId;
use mux::{
    Mux, MuxNotification, SynchronizedOutputAdmissionDecision, SynchronizedOutputDepthOutcome,
    SynchronizedOutputDrainCause, SynchronizedOutputEvent,
};
use mux_lua::MuxPane;
use promise::spawn::sleep;
use std::cell::{RefCell, RefMut};
use std::collections::{HashMap, LinkedList};
use std::ops::Range;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use termwiz::hyperlink::Hyperlink;
use termwiz::surface::SequenceNo;
use wezterm_dynamic::Value;
use wezterm_term::color::ColorPalette;
use wezterm_term::input::LastMouseClick;
use wezterm_term::{Alert, Progress, StableRowIndex, TerminalConfiguration, TerminalSize};

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
    CancelOverlayForPane(PaneId),
    CancelOverlayForTab {
        tab_id: TabId,
        pane_id: Option<PaneId>,
    },
    MuxNotification(MuxNotification),
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
        x >= self.x as isize
            && x <= (self.x + self.width) as isize
            && y >= self.y as isize
            && y <= (self.y + self.height) as isize
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
}

#[derive(Default)]
pub struct PaneState {
    /// If is_some(), the top row of the visible screen.
    /// Otherwise, the viewport is at the bottom of the
    /// scrollback.
    viewport: Option<StableRowIndex>,
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
    fn add_fields<'lua, F: UserDataFields<'lua, Self>>(fields: &mut F) {
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
    fn add_fields<'lua, F: UserDataFields<'lua, Self>>(fields: &mut F) {
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
            pane_id,
            cause,
            bytes,
            depth_outcome,
            max_depth,
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

fn record_sync_output_drain(
    pane_id: PaneId,
    cause: SynchronizedOutputDrainCause,
    bytes: u64,
    maybe_depth_outcome: Option<SynchronizedOutputDepthOutcome>,
    max_depth: u32,
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

    let bytes = if bytes > 0 {
        buffered_bytes_by_pane.remove(&pane_id);
        bytes
    } else {
        buffered_bytes_by_pane.remove(&pane_id).unwrap_or_default()
    };
    let drain_cause = match cause {
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

    if matches!(cause, SynchronizedOutputDrainCause::LiveResizeForce) {
        orchestrator_telemetry
            .record_override(OverrideTrigger::LiveResize, OverrideAction::ForceFlushNow);
    }
    orchestrator_telemetry.record_drain(drain_outcome);

    match cause {
        SynchronizedOutputDrainCause::Esu => {
            let depth_outcome = maybe_depth_outcome
                .map(|outcome| {
                    record_sync_output_depth_outcome(pane_id, outcome, bsu_depth_by_pane)
                })
                .unwrap_or_else(|| {
                    let (outcome, should_remove) = {
                        let depth = bsu_depth_by_pane.entry(pane_id).or_default();
                        let outcome = depth.close_esu();
                        let should_remove = matches!(
                            outcome,
                            frankenterm_core::sync_output_watchdog::BsuDepthOutcome::Flushed
                                | frankenterm_core::sync_output_watchdog::BsuDepthOutcome::Underflow
                        );
                        (outcome, should_remove)
                    };
                    if should_remove {
                        bsu_depth_by_pane.remove(&pane_id);
                    }
                    outcome
                });
            let max_observed = max_depth;
            if matches!(drain_outcome, BufferDrainOutcome::Drained { .. }) {
                frankenterm_core::sync_output_telemetry_bridge::forward_drain(
                    drain_outcome,
                    depth_outcome,
                    max_observed,
                    watchdog_telemetry,
                );
            } else {
                watchdog_telemetry.record_depth_outcome(depth_outcome, max_observed);
            }
        }
        SynchronizedOutputDrainCause::Watchdog => {
            watchdog_telemetry.record_watchdog_decision(WatchdogDecision::ForceFlush);
            if let Some(depth) = bsu_depth_by_pane.get_mut(&pane_id) {
                depth.force_reset();
            }
        }
        SynchronizedOutputDrainCause::LiveResizeForce | SynchronizedOutputDrainCause::Operator => {
            if let Some(depth) = bsu_depth_by_pane.get_mut(&pane_id) {
                depth.force_reset();
            }
        }
    }
}

/// Convert a live `WatchdogedTripleBuffer` health view into the per-pane
/// snapshot shape that `TermWindow` stores for `ft doctor --triple-buffer`.
///
/// The renderer migration owns when to call this; keeping the translation here
/// gives that migration a single GUI-side bridge instead of re-encoding the
/// substrate counters at every frame-timer poll site.
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

    current_modifier_and_leds: (Modifiers, KeyboardLedStatus),
    current_mouse_buttons: Vec<MousePress>,
    current_mouse_capture: Option<MouseCapture>,

    opengl_info: Option<String>,

    /// Keeps track of double and triple clicks
    last_mouse_click: Option<LastMouseClick>,

    /// The URL over which we are currently hovering
    current_highlight: Option<Arc<Hyperlink>>,

    quad_generation: usize,
    shape_generation: usize,
    /// Per-pane render-side dirty-line bitmap (ft-tfzhy / ft-mpc9b.1.2).
    ///
    /// The TermWindow keeps one `DirtyLineBitmap` per `PaneId` so the
    /// render pass can distinguish rows that changed since the last
    /// frame from rows eligible for cached-quad reuse. Coarse
    /// whole-screen events (resize, focus change, font/theme swap)
    /// call `mark_all` on every entry; live PTY dirty ranges, cursor
    /// moves, and selection changes mark row-level damage.
    ///
    /// The `quad_generation` counter above stays as the lower-bound
    /// version on top of per-line dirty for events that genuinely
    /// invalidate everything (font swap, theme change). Both
    /// signals are consumed by the render path together.
    dirty_lines: HashMap<PaneId, render::dirty_lines::DirtyLineBitmap>,
    /// Tracks whether the last dirty-marking event observed by this
    /// TermWindow was a coarse whole-screen invalidation (font swap,
    /// theme change, resize, focus change). Per the
    /// `frankenterm_core::dirty_line_telemetry::should_clear_at_frame_end`
    /// predicate, frame-end `bitmap.clear()` is suppressed on those
    /// frames so the next frame still observes the marks. Defaults
    /// to true so the very first paint pass leaves the bitmaps
    /// marked-all rather than silently clearing them. Per ft-jvj78
    /// (cont of ft-5ykn9). Each whole-screen event source flips this
    /// flag; per-cell sources leave it false.
    last_dirty_event_was_whole_screen: bool,
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
    /// Substrate's `MarksBySource` aggregator: 8 lifetime counts
    /// (one per `DirtyEventSource` variant). Bumped by the
    /// `record_dirty_event` helper at every mark-call site.
    /// Surfaced into `DirtyLineTelemetrySnapshot.marks_by_source`
    /// for ft-doctor's lines-redrawn-by-source breakdown.
    dirty_marks_by_source: frankenterm_core::dirty_line_telemetry::MarksBySource,
    /// Per-pane `TerminalState` triple-buffer owners. The render
    /// path publishes the current visible pane state here; the
    /// ft-r9kr6 follow-up can poll these live owners into the
    /// doctor-facing health snapshots without inventing a second
    /// registry.
    triple_buffer_panes: TerminalStateTripleBufferRegistry,
    /// Per-pane WatchdogedTripleBuffer health snapshots
    /// (ft-gso6n / ft-l0oe3 slice). `triple_buffer_telemetry()`
    /// aggregates these into the substrate's FleetHealthAggregate.
    /// The ft-r9kr6 follow-up owns the status-tick poll from the
    /// live owners above into this map.
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
    shape_cache: RefCell<LfuCache<ShapeCacheKey, anyhow::Result<Rc<Vec<ShapedInfo>>>>>,
    line_to_ele_shape_cache: RefCell<LfuCache<LineToEleShapeCacheKey, LineToElementShapeItem>>,

    line_state_cache: RefCell<LfuCacheU64<Arc<CachedLineState>>>,
    next_line_state_id: u64,

    line_quad_cache: RefCell<LfuCache<LineQuadCacheKey, LineQuadCacheValue>>,

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
    scheduled_animation: RefCell<Option<Instant>>,

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

/// Free-function helper for `TermWindow::clear_dirty_lines_after_frame`
/// so the predicate wiring is unit-testable without needing to
/// stand up a full TermWindow. Per ft-jvj78.
///
/// Consults the substrate's `should_clear_at_frame_end` predicate
/// against the supplied `last_was_whole_screen` flag and the default
/// `DirtyTelemetryConfig`. When the predicate fires, every bitmap is
/// cleared. Either way, the flag is reset to false so the next
/// whole-screen event has to set it explicitly.
/// Per ft-8pcwy: pure predicate the GUI render loop calls per
/// (pane_id, line_idx) to decide whether a row is clean enough to
/// reuse cached quads. Free function so the truth table is
/// unit-testable without standing up a TermWindow.
///
/// Semantics:
/// - gate disabled → never skip (legacy iterate-all-lines).
/// - gate enabled, no bitmap registered → never skip (no per-cell
///   event source has touched this pane yet; safer to render).
/// - gate enabled, bitmap empty → never skip (bitmap was cleared
///   at frame end and no event has marked anything since).
/// - gate enabled, bitmap non-empty → cache-reuse candidate iff
///   the row index is not in the dirty set.
pub(crate) fn should_skip_clean_line(
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

#[must_use]
pub(crate) fn terminal_u16_from_usize(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

#[must_use]
pub(crate) fn terminal_u16_from_stable_delta(value: StableRowIndex) -> u16 {
    u16::try_from(value).unwrap_or(if value < 0 { 0 } else { u16::MAX })
}

#[must_use]
pub(crate) fn terminal_pane_id_to_u64(pane_id: PaneId) -> u64 {
    u64::try_from(pane_id).unwrap_or(u64::MAX)
}

#[must_use]
pub(crate) fn terminal_state_from_rendered_pane(pane: &dyn Pane) -> TerminalState {
    let cursor = pane.get_cursor_position();
    let dims = pane.get_dimensions();

    TerminalState {
        rows: terminal_u16_from_usize(dims.viewport_rows),
        cols: terminal_u16_from_usize(dims.cols),
        cursor_row: terminal_u16_from_stable_delta(cursor.y.saturating_sub(dims.physical_top)),
        cursor_col: terminal_u16_from_usize(cursor.x),
        is_alt_screen: pane.is_alt_screen_active(),
        title: pane.get_title(),
    }
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

fn run_clear_dirty_lines_after_frame(
    bitmaps: &mut HashMap<PaneId, render::dirty_lines::DirtyLineBitmap>,
    last_was_whole_screen: &mut bool,
) {
    let config = frankenterm_core::dirty_line_telemetry::DirtyTelemetryConfig::default();
    let should_clear = frankenterm_core::dirty_line_telemetry::should_clear_at_frame_end(
        *last_was_whole_screen,
        config,
    );
    if should_clear {
        for bitmap in bitmaps.values_mut() {
            bitmap.clear();
        }
    }
    // Always reset — the next whole-screen event must set the flag
    // explicitly via `mark_all_panes_dirty`.
    *last_was_whole_screen = false;
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
                    .get_window(mux_window_id)
                    .map_or(false, |w| w.can_close_without_prompting());
                if can_close {
                    mux.kill_window(self.mux_window_id);
                    window.close();
                    if let Some(front_end) = try_front_end() {
                        front_end.forget_known_window(window);
                    }
                    return;
                }
                let window = window.clone();
                let (overlay, future) = match start_overlay(self, &tab, move |tab_id, term| {
                    confirm_close_window(term, mux_window_id, window, tab_id)
                }) {
                    Ok(overlay) => overlay,
                    Err(err) => {
                        log::error!("failed to start close-window overlay: {err:#}");
                        return;
                    }
                };
                self.assign_overlay(tab.tab_id(), overlay);
                promise::spawn::spawn(future).detach();

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
    /// Bitmaps that have never been registered (capacity 0) absorb
    /// the call as a no-op — the next read by the render path will
    /// resize them to match the pane's visible rows.
    /// Mark every registered pane bitmap dirty (whole-screen event:
    /// font/theme swap, focus change, resize). Per ft-jvj78 (cont
    /// of ft-5ykn9): also flips `last_dirty_event_was_whole_screen`
    /// so the next frame-end clear is suppressed and the marks
    /// survive into the upcoming paint pass.
    pub fn mark_all_panes_dirty(&mut self) {
        for bitmap in self.dirty_lines.values_mut() {
            bitmap.mark_all();
        }
        self.last_dirty_event_was_whole_screen = true;
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
        bitmap
    }

    /// Drop the bitmap for a pane that has been closed. Without
    /// this the HashMap would leak entries for every pane the user
    /// has ever opened.
    pub fn forget_dirty_lines_for_pane(&mut self, pane_id: PaneId) {
        self.dirty_lines.remove(&pane_id);
    }

    /// Frame-end hook: clear every per-pane dirty bitmap iff the
    /// substrate's `should_clear_at_frame_end` predicate says so.
    /// Coarse whole-screen events (font/theme/resize/focus) leave
    /// the marks across the boundary so the next frame still sees
    /// them. Per ft-jvj78 (cont of ft-5ykn9).
    ///
    /// Called from `paint_impl` after the frame has been submitted.
    pub fn clear_dirty_lines_after_frame(&mut self) {
        run_clear_dirty_lines_after_frame(
            &mut self.dirty_lines,
            &mut self.last_dirty_event_was_whole_screen,
        );
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
    /// - mark_all_panes_dirty_with_source (FocusChange / ThemeSwap / FontSwap / Resize)
    pub fn record_dirty_event(
        &mut self,
        source: frankenterm_core::dirty_line_telemetry::DirtyEventSource,
    ) {
        self.dirty_marks_by_source.record(source);
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
        self.record_dirty_event(source);
    }

    pub fn publish_terminal_state_snapshot(
        &mut self,
        pane_id: u64,
        state: TerminalState,
    ) -> frankenterm_core::triple_buffer::PublishOutcome {
        self.triple_buffer_panes.publish(pane_id, state)
    }

    fn publish_terminal_state_for_pane(
        &mut self,
        pos: &PositionedPane,
    ) -> frankenterm_core::triple_buffer::PublishOutcome {
        self.publish_terminal_state_snapshot(
            terminal_pane_id_to_u64(pos.pane.pane_id()),
            terminal_state_from_rendered_pane(&*pos.pane),
        )
    }

    /// Per ft-kyail: drop the live triple-buffer owner for a closed pane and
    /// clear the last retained health snapshot for the same pane. This keeps
    /// ownership and doctor telemetry lifetimes aligned.
    pub fn forget_terminal_state_buffer_for_pane(&mut self, pane_id: u64) {
        self.triple_buffer_panes.remove(pane_id);
        self.forget_pane_health_snapshot(pane_id);
    }

    #[must_use]
    pub fn terminal_state_buffer_pane_count(&self) -> usize {
        self.triple_buffer_panes.len()
    }

    /// Per ft-gso6n / ft-l0oe3 slice: store the most-recent
    /// per-pane health snapshot from the WatchdogedTripleBuffer.
    /// Called by the integration's frame-timer poll each tick.
    ///
    /// Pane removal: the dirty_lines forget hook also clears the
    /// triple-buffer owner and snapshot for the same pane.
    pub fn record_pane_health_snapshot(
        &mut self,
        pane_id: u64,
        snapshot: frankenterm_core::triple_buffer_fleet_health::PaneHealthSnapshot,
    ) {
        self.triple_buffer_pane_health.insert(pane_id, snapshot);
    }

    /// Per ft-71v6n: record a live `WatchdogedTripleBuffer`
    /// health view from the renderer poll path. This keeps the
    /// frame-timer integration from having to know the exact
    /// `PaneHealthSnapshot` field mapping.
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
    /// `None` when no bitmap has been registered for the pane (the
    /// render loop falls back to legacy iterate-all-lines in that
    /// case via `should_skip_clean_line`).
    pub fn peek_dirty_lines(
        &self,
        pane_id: PaneId,
    ) -> Option<&render::dirty_lines::DirtyLineBitmap> {
        self.dirty_lines.get(&pane_id)
    }

    /// Per ft-8pcwy: bump the per-pane clean-line skip counter.
    /// Called by the render loop when `should_skip_clean_line`
    /// returned true and a render_line call was elided.
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
        self.quad_generation += 1;
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
        self.record_quad_buffer_allocation_snapshot(0);

        Ok(())
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum WebGpuSurfaceErrorAction {
    Retry,
    SkipFrame,
    Fail,
}

fn classify_webgpu_surface_error(err: &wgpu::SurfaceError) -> WebGpuSurfaceErrorAction {
    match err {
        wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated => WebGpuSurfaceErrorAction::Retry,
        wgpu::SurfaceError::Timeout => WebGpuSurfaceErrorAction::SkipFrame,
        wgpu::SurfaceError::OutOfMemory | wgpu::SurfaceError::Other => {
            WebGpuSurfaceErrorAction::Fail
        }
    }
}

impl TermWindow {
    pub async fn new_window(mux_window_id: MuxWindowId) -> anyhow::Result<()> {
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

        let window_background = load_background_image(&config, &dimensions, &render_metrics);

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
            last_mouse_click: None,
            current_highlight: None,
            quad_generation: 0,
            shape_generation: 0,
            dirty_lines: HashMap::new(),
            // Per ft-jvj78: first paint is treated as a whole-screen
            // event so the dirty bitmap is not silently cleared
            // before any pane has had a chance to populate it.
            last_dirty_event_was_whole_screen: true,
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
            scheduled_animation: RefCell::new(None),
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
            window.show();
            myself.subscribe_to_pane_updates();
            myself.emit_window_event("window-config-reloaded", None);
            myself.emit_status_event();
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
                self.resizes_pending -= 1;
                if self.is_repaint_pending {
                    self.is_repaint_pending = false;
                    if self.webgpu.is_some() {
                        self.do_paint_webgpu()?;
                    } else {
                        self.do_paint(window);
                    }
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
                } else if self.webgpu.is_some() {
                    self.do_paint_webgpu()
                } else {
                    Ok(self.do_paint(window))
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

    fn do_paint(&mut self, window: &Window) -> bool {
        let gl = match self.gl.as_ref() {
            Some(gl) => gl,
            None => return false,
        };

        if gl.is_context_lost() {
            log::error!("opengl context was lost; should reinit");
            window.close();
            if let Some(front_end) = try_front_end() {
                front_end.forget_known_window(window);
            }
            return false;
        }

        let mut frame = glium::Frame::new(
            Rc::clone(&gl),
            (
                self.dimensions.pixel_width as u32,
                self.dimensions.pixel_height as u32,
            ),
        );
        self.paint_impl(&mut RenderFrame::Glium(&mut frame));
        window.finish_frame(frame).is_ok()
    }

    fn do_paint_webgpu(&mut self) -> anyhow::Result<bool> {
        let Some(webgpu) = self.webgpu.as_mut() else {
            log::warn!("cannot paint webgpu frame before webgpu state is initialized");
            return Ok(false);
        };
        webgpu.resize(self.dimensions);
        match self.do_paint_webgpu_impl() {
            Ok(ok) => Ok(ok),
            Err(err) => {
                match err
                    .downcast_ref::<wgpu::SurfaceError>()
                    .map(classify_webgpu_surface_error)
                {
                    Some(WebGpuSurfaceErrorAction::Retry) => {
                        log::warn!("webgpu surface became stale; retrying after resize");
                        let Some(webgpu) = self.webgpu.as_mut() else {
                            log::warn!("cannot retry webgpu frame: webgpu state is gone");
                            return Ok(false);
                        };
                        webgpu.resize(self.dimensions);
                        return self.do_paint_webgpu_impl();
                    }
                    Some(WebGpuSurfaceErrorAction::SkipFrame) => {
                        log::warn!("webgpu surface timed out acquiring the next frame; skipping");
                        if let Some(window) = self.window.as_ref() {
                            window.invalidate();
                        }
                        return Ok(true);
                    }
                    Some(WebGpuSurfaceErrorAction::Fail) | None => {}
                }
                Err(err)
            }
        }
    }

    fn do_paint_webgpu_impl(&mut self) -> anyhow::Result<bool> {
        self.paint_impl(&mut RenderFrame::WebGpu);
        Ok(true)
    }

    fn dispatch_notif(&mut self, notif: TermWindowNotif, window: &Window) -> anyhow::Result<()> {
        fn chan_err<T>(e: TrySendError<T>) -> anyhow::Error {
            anyhow::anyhow!("{}", e)
        }

        match notif {
            TermWindowNotif::InvalidateShapeCache => {
                self.shape_generation += 1;
                self.shape_cache.borrow_mut().clear();
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
            TermWindowNotif::CancelOverlayForPane(pane_id) => {
                self.cancel_overlay_for_pane(pane_id);
            }
            TermWindowNotif::CancelOverlayForTab { tab_id, pane_id } => {
                self.cancel_overlay_for_tab(tab_id, pane_id);
            }
            TermWindowNotif::MuxNotification(n) => match n {
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
                    let Some(mux) = Mux::try_get() else {
                        log::warn!("cannot size added tab {tab_id}: mux is no longer active");
                        return Ok(());
                    };
                    let mut size = self.terminal_size;
                    if let Some(tab) = mux.get_tab(tab_id) {
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
                MuxNotification::WindowInvalidated(_) => {
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
                }
                MuxNotification::PaneAdded(_)
                | MuxNotification::WorkspaceRenamed { .. }
                | MuxNotification::WindowWorkspaceChanged(_)
                | MuxNotification::ActiveWorkspaceChanged(_)
                | MuxNotification::Empty
                | MuxNotification::WindowCreated(_) => {}
            },
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
            .filter_map(|(_, state)| state.overlay.as_ref().map(|overlay| overlay.pane.pane_id()))
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

    fn mux_pane_output_event_callback(
        n: MuxNotification,
        window: &Window,
        mux_window_id: MuxWindowId,
        dead: &Arc<AtomicBool>,
    ) -> bool {
        if dead.load(Ordering::Relaxed) {
            // Subscription cancelled asynchronously
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
                let Some(mux) = Mux::try_get() else {
                    log::debug!("PaneOutput: mux no longer active, cancel mux subscription");
                    return false;
                };
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
                return Mux::try_get()
                    .map(|mux| mux.get_window(mux_window_id).is_some())
                    .unwrap_or(false);
            }
            MuxNotification::TabAddedToWindow { window_id, .. }
            | MuxNotification::WindowTitleChanged { window_id, .. }
            | MuxNotification::WindowInvalidated(window_id) => {
                if window_id != mux_window_id {
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
                if Mux::try_get().and_then(|mux| mux.window_containing_tab(tab_id))
                    == Some(mux_window_id)
                {
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
            | MuxNotification::WindowWorkspaceChanged(_) => return true,
            MuxNotification::Alert {
                alert: Alert::PaletteChanged { .. },
                ..
            } => {
                // fall through
            }
        }

        window.notify(TermWindowNotif::MuxNotification(n));

        true
    }

    fn subscribe_to_pane_updates(&self) {
        let Some(window) = self.window.clone() else {
            log::warn!("cannot subscribe to pane updates without a GUI window");
            return;
        };
        let mux_window_id = Arc::clone(&self.mux_window_id_for_subscriptions);
        let Some(mux) = Mux::try_get() else {
            log::warn!("cannot subscribe to pane updates without an active mux");
            return;
        };
        let dead = Arc::new(AtomicBool::new(false));
        mux.subscribe(move |n| {
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
            let window = window.clone();
            let dead = dead.clone();
            promise::spawn::spawn_into_main_thread(async move {
                Self::mux_pane_output_event_callback(n, &window, mux_window_id, &dead)
            })
            .detach();
            true
        });
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

                if let Err(err) = config::lua::emit_event(&lua, (name.clone(), args)).await {
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

        promise::spawn::spawn(config::with_lua_config_on_main_thread(move |lua| {
            do_event(lua, name, window, pane)
        }))
        .detach();
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

    fn check_for_dirty_lines_and_invalidate_selection(&mut self, pane: &Arc<dyn Pane>) {
        let dims = pane.get_dimensions();
        let viewport = self
            .get_viewport(pane.pane_id())
            .unwrap_or(dims.physical_top);
        let visible_range = viewport..viewport + dims.viewport_rows as StableRowIndex;
        let seqno = self.selection(pane.pane_id()).seqno;
        let dirty = pane.get_changed_since(visible_range, seqno);

        if dirty.is_empty() {
            return;
        }

        // Per ft-camu6 (cont of ft-jvj78): wire PTY-write dirty
        // marks into the per-pane DirtyLineBitmap. The term layer's
        // `get_changed_since` returns stable-row indices; translate
        // each to its visible-row index (stable - viewport) and mark
        // it dirty in the bitmap. Out-of-bounds rows (e.g., scrolled
        // past the viewport) are silently dropped by
        // DirtyLineBitmap::mark per its existing contract.
        let pane_id = pane.pane_id();
        let viewport_rows = dims.viewport_rows;
        let bitmap = self.dirty_lines_for_pane(pane_id, viewport_rows);
        mark_stable_row_ranges_dirty(bitmap, viewport, dirty.iter().cloned());
        // Per ft-i6k6u: tag the mark with its source so the
        // substrate's per-source aggregator attributes
        // PTY-driven seqno bumps separately from selection /
        // theme / font / focus events. The actual translation
        // already happened above; here we just bump the counter.
        self.record_dirty_event(frankenterm_core::dirty_line_telemetry::DirtyEventSource::Pty);

        if pane.downcast_ref::<CopyOverlay>().is_none()
            && pane.downcast_ref::<QuickSelectOverlay>().is_none()
        {
            // If any of the changed lines intersect with the
            // selection, then we need to clear the selection, but not
            // when the search overlay is active; the search overlay
            // marks lines as dirty to force invalidate them for
            // highlighting purpose but also manipulates the selection
            // and we want to allow it to retain the selection it made!

            let (clear_selection, cleared_rows) =
                if let Some(selection_range) = self.selection(pane.pane_id()).range.as_ref() {
                    let selection_rows = selection_range.rows();
                    let intersects = selection_rows
                        .clone()
                        .into_iter()
                        .any(|row| dirty.contains(row));
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

            if clear_selection {
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
        self.shape_generation += 1;
        {
            let mut shape_cache = self.shape_cache.borrow_mut();
            shape_cache.update_config(&config);
            shape_cache.clear();
        }
        self.line_state_cache.borrow_mut().update_config(&config);
        self.line_quad_cache.borrow_mut().update_config(&config);
        self.line_to_ele_shape_cache
            .borrow_mut()
            .update_config(&config);
        self.fancy_tab_bar.take();
        self.invalidate_fancy_tab_bar();
        self.invalidate_modal();
        self.input_map = InputMap::new(&config);
        self.leader_is_down = None;
        self.render_state.as_mut().map(|rs| rs.config_changed());
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

        // Do this after we've potentially adjusted scaling based on config/padding
        // and window size
        self.window_background = reload_background_image(
            &config,
            &self.window_background,
            &self.dimensions,
            &self.render_metrics,
        );

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
                if let Err(err) =
                    config::lua::emit_event(&lua, ("user-var-changed".to_string(), args)).await
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

        promise::spawn::spawn(config::with_lua_config_on_main_thread(move |lua| {
            do_event(lua, name, value, window, pane)
        }))
        .detach();
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
                self.last_status_call = target;

                let window = window.clone();
                promise::spawn::spawn(async move {
                    sleep(target.saturating_duration_since(Instant::now())).await;
                    window.notify(TermWindowNotif::EmitStatusUpdate);
                })
                .detach();
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
            promise::spawn::spawn(async move {
                // ~one frame at 60 Hz. Imperceptible delay for OSC-driven UI.
                sleep(Duration::from_millis(16)).await;
                window.notify(TermWindowNotif::Apply(Box::new(|tw| {
                    tw.pending_update_title = false;
                    tw.update_title();
                })));
            })
            .detach();
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
        let mut window = mux
            .get_window_mut(self.mux_window_id)
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
            window.save_and_then_set_active(tab_idx);

            drop(window);

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
        let mut window = mux
            .get_window_mut(self.mux_window_id)
            .ok_or_else(|| anyhow!("no such window"))?;

        let max = window.len();
        ensure!(max > 0, "no more tabs");

        let active = window.get_active_idx();

        ensure!(tab_idx < max, "cannot move a tab out of range");

        let tab_inst = window.remove_by_idx(active);
        window.insert(tab_idx, &tab_inst);
        window.set_active_without_saving(tab_idx);

        drop(window);
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

        let (overlay, future) = match start_overlay(self, &tab, move |_tab_id, term| {
            crate::overlay::selector::selector(term, args, gui_win, pane)
        }) {
            Ok(overlay) => overlay,
            Err(err) => {
                log::error!("failed to start input-selector overlay: {err:#}");
                return;
            }
        };
        self.assign_overlay(tab.tab_id(), overlay);
        promise::spawn::spawn(future).detach();
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

        let (overlay, future) = match start_overlay(self, &tab, move |_tab_id, term| {
            crate::overlay::prompt::show_line_prompt_overlay(term, args, gui_win, pane)
        }) {
            Ok(overlay) => overlay,
            Err(err) => {
                log::error!("failed to start prompt overlay: {err:#}");
                return;
            }
        };
        self.assign_overlay(tab.tab_id(), overlay);
        promise::spawn::spawn(future).detach();
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

        let (overlay, future) = match start_overlay(self, &tab, move |_tab_id, term| {
            crate::overlay::confirm::show_confirmation_overlay(term, args, gui_win, pane)
        }) {
            Ok(overlay) => overlay,
            Err(err) => {
                log::error!("failed to start confirmation overlay: {err:#}");
                return;
            }
        };
        self.assign_overlay(tab.tab_id(), overlay);
        promise::spawn::spawn(future).detach();
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

        let (overlay, future) = match start_overlay(self, &tab, move |_tab_id, term| {
            crate::overlay::show_debug_overlay(term, gui_win, opengl_info, connection_info)
        }) {
            Ok(overlay) => overlay,
            Err(err) => {
                log::error!("failed to start debug overlay: {err:#}");
                return;
            }
        };
        self.assign_overlay(tab.tab_id(), overlay);
        promise::spawn::spawn(future).detach();
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

        promise::spawn::spawn(async move {
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
                    let (overlay, future) =
                        match start_overlay(term_window, &tab, move |_tab_id, term| {
                            launcher(args, term, window, initial_choice_idx)
                        }) {
                            Ok(overlay) => overlay,
                            Err(err) => {
                                log::error!("failed to start launcher overlay: {err:#}");
                                return;
                            }
                        };

                    term_window.assign_overlay(tab_id, overlay);
                    promise::spawn::spawn(future).detach();
                }
            })));
        })
        .detach();
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
            let Some(pane_id) = u32::try_from(floating.pane_id).ok() else {
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
            controller.set_floating(pane_id, rect);
        }
        if let Some(focused) = panes.iter().find(|pane| pane.is_focused) {
            if let Ok(pane_id) = u32::try_from(focused.pane_id) {
                controller.focus(pane_id);
            }
        }
        let _ = controller.drain_a11y_messages();
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
            tab.set_floating_pane_z_order(entry.pane_id as usize, entry.z_order);
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

        if command == FloatingKeyboardCommand::CycleOverlapping {
            let x = self.last_mouse_coords.0.min(u16::MAX as usize) as u16;
            let y = self.last_mouse_coords.1.max(0).min(i64::from(u16::MAX)) as u16;
            let Some(next) = controller.cycle_overlapping_at(x, y) else {
                return false;
            };
            let changed = tab.set_floating_pane_focus(next as usize);
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

        let changed = match position {
            PanePosition::Floating(rect) => tab
                .set_floating_pane_rect(focused as usize, Self::mux_floating_rect(rect))
                .is_some(),
            PanePosition::Tiled if command == FloatingKeyboardCommand::TogglePin => {
                tab.remove_floating_pane(focused as usize).is_some()
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

                        let window = self.window.clone().ok_or_else(|| {
                            anyhow::anyhow!("cannot start quit confirmation without a GUI window")
                        })?;
                        let (overlay, future) = start_overlay(self, &tab, move |tab_id, term| {
                            confirm_quit_program(term, window, tab_id)
                        })?;
                        self.assign_overlay(tab.tab_id(), overlay);
                        promise::spawn::spawn(future).detach();
                    }
                }
            }
            SelectTextAtMouseCursor(mode) => self.select_text_at_mouse_cursor(*mode, pane),
            ExtendSelectionToMouseCursor(mode) => {
                self.extend_selection_at_mouse_cursor(*mode, pane)
            }
            ClearSelection => {
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
                let activity = crate::Activity::new();
                let Some(mux) = self.mux_or_log("switch workspace") else {
                    return Ok(PerformAssignmentResult::Handled);
                };
                let name = name
                    .as_ref()
                    .map(|name| name.to_string())
                    .unwrap_or_else(|| mux.generate_workspace_name());
                let Some(switcher) = crate::frontend::WorkspaceSwitcher::new(&name) else {
                    return Ok(PerformAssignmentResult::Handled);
                };
                mux.set_active_workspace(&name);

                if mux.iter_windows_in_workspace(&name).is_empty() {
                    let spawn = spawn.as_ref().map(|s| s.clone()).unwrap_or_default();
                    let size = self.terminal_size;
                    let term_config = Arc::new(TermConfig::with_config(self.config.clone()));
                    let src_window_id = self.mux_window_id;

                    promise::spawn::spawn(async move {
                        if let Err(err) = crate::spawn::spawn_command_internal(
                            spawn,
                            SpawnWhere::NewWindow,
                            size,
                            Some(src_window_id),
                            term_config,
                        )
                        .await
                        {
                            log::error!("Failed to spawn: {:#}", err);
                        }
                        switcher.do_switch();
                        drop(activity);
                    })
                    .detach();
                } else {
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
                let domain = domain.to_string();
                let dpi = self.dimensions.dpi as u32;

                promise::spawn::spawn(async move {
                    let mux = Mux::try_get()
                        .ok_or_else(|| anyhow!("cannot attach domain without an active mux"))?;
                    let domain = mux
                        .get_domain_by_name(&domain)
                        .ok_or_else(|| anyhow!("{} is not a valid domain name", domain))?;
                    domain.attach(Some(window)).await?;

                    let have_panes_in_domain = mux
                        .iter_panes()
                        .iter()
                        .any(|p| p.domain_id() == domain.domain_id());

                    if !have_panes_in_domain {
                        let config = config::configuration();
                        let _tab = domain
                            .spawn(
                                config.initial_size(
                                    dpi,
                                    Some(crate::cell_pixel_dims(&config, dpi as f64)?),
                                ),
                                None,
                                None,
                                window,
                            )
                            .await?;
                    }

                    Result::<(), anyhow::Error>::Ok(())
                })
                .detach();
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
                    if let Ok(core_pane_id) = u32::try_from(pane_id) {
                        controller.focus(core_pane_id);
                        controller.apply_keyboard_command(
                            FloatingKeyboardCommand::TogglePin,
                            tab.get_size().cols.min(u16::MAX as usize) as u16,
                            tab.get_size().rows.min(u16::MAX as usize) as u16,
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
                let modal = crate::termwindow::palette::CommandPalette::new(self);
                self.set_modal(Rc::new(modal));
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
                        config::lua::emit_event(&lua, ("open-uri".to_string(), args))
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

            promise::spawn::spawn(config::with_lua_config_on_main_thread(move |lua| {
                open_uri(lua, window, pane, link.uri().to_string())
            }))
            .detach();
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
            let window = match self.gui_window_or_log("start close-pane overlay") {
                Some(window) => window,
                None => return,
            };
            let (overlay, future) = match start_overlay_pane(self, &pane, move |pane_id, term| {
                confirm_close_pane(pane_id, term, mux_window_id, window)
            }) {
                Ok(overlay) => overlay,
                Err(err) => {
                    log::error!("failed to start close-pane overlay: {err:#}");
                    return;
                }
            };
            self.assign_overlay_for_pane(pane_id, overlay);
            promise::spawn::spawn(future).detach();
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

            let window = match self.gui_window_or_log("start close-tab overlay") {
                Some(window) => window,
                None => return,
            };
            let (overlay, future) = match start_overlay(self, &tab, move |tab_id, term| {
                confirm_close_tab(tab_id, term, mux_window_id, window)
            }) {
                Ok(overlay) => overlay,
                Err(err) => {
                    log::error!("failed to start close-tab overlay: {err:#}");
                    return;
                }
            };
            self.assign_overlay(tab_id, overlay);
            promise::spawn::spawn(future).detach();
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
        let mux_window_id = self.mux_window_id;
        if confirm && !tab.can_close_without_prompting(CloseReason::Tab) {
            let window = match self.gui_window_or_log("start close-current-tab overlay") {
                Some(window) => window,
                None => return,
            };
            let (overlay, future) = match start_overlay(self, &tab, move |tab_id, term| {
                confirm_close_tab(tab_id, term, mux_window_id, window)
            }) {
                Ok(overlay) => overlay,
                Err(err) => {
                    log::error!("failed to start close-current-tab overlay: {err:#}");
                    return;
                }
            };
            self.assign_overlay(tab_id, overlay);
            promise::spawn::spawn(future).detach();
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
        RefMut::map(self.tab_state.borrow_mut(), |state| {
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
        }
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
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
        self.pane_state(pane.pane_id()).viewport = None;
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

    /// if pane_id.is_none(), removes any overlay for the specified tab.
    /// Otherwise: if the overlay is the specified pane for that tab, remove it.
    fn remove_overlay_pane_from_mux(&self, pane_id: PaneId) {
        match Mux::try_get() {
            Some(mux) => mux.remove_pane(pane_id),
            None => log::warn!("cannot remove overlay pane {pane_id}: mux is no longer active"),
        }
    }

    fn cancel_overlay_for_tab(&mut self, tab_id: TabId, pane_id: Option<PaneId>) {
        if pane_id.is_some() {
            let current = self
                .tab_state(tab_id)
                .overlay
                .as_ref()
                .map(|o| o.pane.pane_id());
            if current != pane_id {
                return;
            }
        }
        if let Some(overlay) = self.tab_state(tab_id).overlay.take() {
            self.remove_overlay_pane_from_mux(overlay.pane.pane_id());
        }
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    pub fn schedule_cancel_overlay(window: Window, tab_id: TabId, pane_id: Option<PaneId>) {
        window.notify(TermWindowNotif::CancelOverlayForTab { tab_id, pane_id });
    }

    fn cancel_overlay_for_pane(&mut self, pane_id: PaneId) {
        if let Some(overlay) = self.pane_state(pane_id).overlay.take() {
            // Ungh, when I built the CopyOverlay, its pane doesn't get
            // added to the mux and instead it reports the overlaid
            // pane id.  Take care to avoid killing ourselves off
            // when closing the CopyOverlay
            if pane_id != overlay.pane.pane_id() {
                self.remove_overlay_pane_from_mux(overlay.pane.pane_id());
            }
        }
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    pub fn schedule_cancel_overlay_for_pane(window: Window, pane_id: PaneId) {
        window.notify(TermWindowNotif::CancelOverlayForPane(pane_id));
    }

    pub fn assign_overlay_for_pane(&mut self, pane_id: PaneId, pane: Arc<dyn Pane>) {
        self.cancel_overlay_for_pane(pane_id);
        self.pane_state(pane_id).overlay.replace(OverlayState {
            pane,
            key_table_state: KeyTableState::default(),
        });
        self.update_title();
    }

    pub fn assign_overlay(&mut self, tab_id: TabId, overlay: Arc<dyn Pane>) {
        self.cancel_overlay_for_tab(tab_id, None);
        self.tab_state(tab_id).overlay.replace(OverlayState {
            pane: overlay,
            key_table_state: KeyTableState::default(),
        });
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
        SyncOutputDoctorSnapshot, WebGpuSurfaceErrorAction, a11y_op_kind_from_frame_budget_op,
        base_policy_for_frame_budget_state, classify_webgpu_surface_error,
        default_frame_budget_cost_ns, evaluate_frame_budget_reduce_motion_gate, frame_budget,
        mark_cursor_rows_dirty, mark_stable_row_ranges_dirty, mark_stable_rows_dirty,
        pane_health_snapshot_from_watchdoged_health, record_drained_frame_budget_ops,
        record_frame_budget_execution_outstanding, record_sync_output_mux_event,
        reduce_motion_state_from_preference, render, run_clear_dirty_lines_after_frame,
        should_force_paint_for_frame_budget, should_run_frame_budget_decision,
        should_skip_clean_line, terminal_pane_id_to_u64, terminal_u16_from_stable_delta,
        terminal_u16_from_usize,
    };

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

    #[test]
    fn terminal_state_numeric_fields_saturate_for_gui_publish() {
        assert_eq!(terminal_u16_from_usize(24), 24);
        assert_eq!(terminal_u16_from_usize(usize::MAX), u16::MAX);
        assert_eq!(terminal_u16_from_stable_delta(10), 10);
        assert_eq!(terminal_u16_from_stable_delta(-1), 0);
        assert_eq!(terminal_u16_from_stable_delta(isize::MAX), u16::MAX);
        assert_eq!(terminal_pane_id_to_u64(7), 7);
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
        assert_eq!(m.pty, 2);
        assert_eq!(m.cursor_move, 1);
        assert_eq!(m.selection_change, 1);
        assert_eq!(m.theme_swap, 1);
        assert_eq!(m.font_swap, 1);
        assert_eq!(m.status_tile_update, 1);
        assert_eq!(m.focus_change, 1);
        assert_eq!(m.resize, 1);
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
    /// 4 whole-screen variants stay aligned with the wiring sites.
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
        assert!(!should_skip_clean_line(false, Some(&bm), 0));
        assert!(!should_skip_clean_line(false, Some(&bm), 5));
        assert!(!should_skip_clean_line(false, None, 0));
    }

    /// ft-8pcwy: gate enabled but no bitmap registered → never
    /// skip. Per-cell event sources haven't touched the pane yet;
    /// safer to render than to leave a hole.
    #[test]
    fn skip_predicate_off_when_no_bitmap_registered() {
        assert!(!should_skip_clean_line(true, None, 0));
        assert!(!should_skip_clean_line(true, None, 23));
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
                !should_skip_clean_line(true, Some(&bm), idx),
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
            let should_skip = should_skip_clean_line(true, Some(&bm), idx);
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
        assert!(!should_skip_clean_line(true, Some(&bm), 7));
        // idx within capacity, clean.
        assert!(should_skip_clean_line(true, Some(&bm), 6));
        // idx past capacity → contains() returns false → skip.
        // (Out-of-range rows shouldn't be passed in by the render
        //  loop, but the predicate is still well-defined.)
        assert!(should_skip_clean_line(true, Some(&bm), 8));
        assert!(should_skip_clean_line(true, Some(&bm), usize::MAX));
    }

    /// ft-jvj78: when no whole-screen event has happened, the
    /// frame-end clear runs on every pane bitmap and the flag is
    /// idle.
    #[test]
    fn frame_end_clear_runs_on_per_cell_frames() {
        let mut bitmaps: HashMap<usize, render::dirty_lines::DirtyLineBitmap> = HashMap::new();
        let mut bm = render::dirty_lines::DirtyLineBitmap::new(24);
        bm.mark(5);
        bm.mark(7);
        bitmaps.insert(1, bm);
        let mut flag = false;

        run_clear_dirty_lines_after_frame(&mut bitmaps, &mut flag);

        let cleared = bitmaps.get(&1).expect("pane 1 bitmap retained");
        assert_eq!(cleared.count(), 0, "frame-end must clear marks");
        assert_eq!(
            cleared.frames_cleared_total(),
            1,
            "lifetime clear counter must increment exactly once",
        );
        assert!(!flag, "flag must remain false after a clean frame");
    }

    /// ft-jvj78: a whole-screen event suppresses the next frame-end
    /// clear so the marks survive into the upcoming paint pass. The
    /// flag is consumed (reset to false) so a single event only
    /// suppresses one clear.
    #[test]
    fn whole_screen_event_suppresses_next_clear_then_resets() {
        let mut bitmaps: HashMap<usize, render::dirty_lines::DirtyLineBitmap> = HashMap::new();
        let mut bm = render::dirty_lines::DirtyLineBitmap::new(24);
        bm.mark_all();
        bitmaps.insert(1, bm);
        let mut flag = true;

        run_clear_dirty_lines_after_frame(&mut bitmaps, &mut flag);

        // Whole-screen event suppressed the clear.
        let kept = bitmaps.get(&1).expect("pane 1 bitmap retained");
        assert_eq!(kept.count(), 24, "whole-screen frame must keep marks");
        assert_eq!(
            kept.frames_cleared_total(),
            0,
            "no clear means lifetime counter stays at 0",
        );
        // But the flag is consumed.
        assert!(!flag, "flag must be reset after consumption");

        // Now the next call (no new whole-screen event) does clear.
        run_clear_dirty_lines_after_frame(&mut bitmaps, &mut flag);
        let cleared = bitmaps.get(&1).expect("pane 1 bitmap retained");
        assert_eq!(cleared.count(), 0);
        assert_eq!(cleared.frames_cleared_total(), 1);
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
        let mut flag = false;

        run_clear_dirty_lines_after_frame(&mut bitmaps, &mut flag);

        for pane_id in 1..=4 {
            let bm = bitmaps.get(&pane_id).expect("bitmap retained");
            assert_eq!(bm.count(), 0, "pane {pane_id} must be cleared");
            assert_eq!(bm.frames_cleared_total(), 1);
        }
    }

    #[test]
    fn webgpu_surface_error_classification_retries_stale_surfaces() {
        assert_eq!(
            classify_webgpu_surface_error(&wgpu::SurfaceError::Lost),
            WebGpuSurfaceErrorAction::Retry
        );
        assert_eq!(
            classify_webgpu_surface_error(&wgpu::SurfaceError::Outdated),
            WebGpuSurfaceErrorAction::Retry
        );
    }

    #[test]
    fn webgpu_surface_error_classification_skips_timeout_frames() {
        assert_eq!(
            classify_webgpu_surface_error(&wgpu::SurfaceError::Timeout),
            WebGpuSurfaceErrorAction::SkipFrame
        );
    }

    #[test]
    fn webgpu_surface_error_classification_fails_terminal_errors() {
        assert_eq!(
            classify_webgpu_surface_error(&wgpu::SurfaceError::OutOfMemory),
            WebGpuSurfaceErrorAction::Fail
        );
        assert_eq!(
            classify_webgpu_surface_error(&wgpu::SurfaceError::Other),
            WebGpuSurfaceErrorAction::Fail
        );
    }
}
