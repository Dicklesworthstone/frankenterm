use crate::selection::{SelectionCoordinate, SelectionRange, SelectionX};
use crate::termwindow::keyevent::KeyTableArgs;
use crate::termwindow::{TermWindow, TermWindowNotif};
use config::keyassignment::{
    ClipboardCopyDestination, CopyModeAssignment, KeyAssignment, KeyTable, KeyTableEntry,
    ScrollbackEraseMode, SelectionMode,
};
use futures::channel::oneshot;
use futures::future::{AbortHandle, Abortable};
use mux::domain::DomainId;
use mux::pane::{
    CachePolicy, ForEachPaneLogicalLine, LogicalLine, Pane, PaneId, Pattern, PatternType,
    PerformAssignmentResult, SearchResult, WithPaneLines,
};
use mux::renderable::*;
use mux::tab::TabId;
use ordered_float::NotNan;
use parking_lot::{MappedMutexGuard, Mutex, MutexGuard};
use promise::spawn::sleep;
use rangeset::RangeSet;
use rayon::prelude::*;
use std::collections::{HashMap, VecDeque};
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use termwiz::cell::{Cell, CellAttributes};
use termwiz::color::AnsiColor;
use termwiz::lineedit::{LineEditBuffer, Movement};
use termwiz::surface::{CursorVisibility, SEQ_ZERO, SequenceNo};
use unicode_segmentation::*;
use url::Url;
use wezterm_term::color::ColorPalette;
use wezterm_term::{
    Clipboard, KeyCode, KeyModifiers, Line, MouseEvent, SemanticType, StableRowIndex, TerminalSize,
    unicode_column_width,
};
use window::{KeyCode as WKeyCode, Modifiers, WindowOps};

lazy_static::lazy_static! {
    static ref SAVED_PATTERN: Mutex<HashMap<TabId, Pattern>> = Mutex::new(HashMap::new());
}

const SEARCH_CHUNK_SIZE: StableRowIndex = 1000;
// Local search joins at most 1,024 cells into a logical segment. One row
// per cell is the conservative overlap, including one-column terminals.
const SEARCH_CHUNK_CONTEXT: StableRowIndex = 1024;

fn older_search_chunk(
    previous_start: StableRowIndex,
    history: Range<StableRowIndex>,
) -> Range<StableRowIndex> {
    previous_start
        .saturating_sub(SEARCH_CHUNK_SIZE)
        .max(history.start)
        ..previous_start
            .saturating_add(SEARCH_CHUNK_CONTEXT)
            .min(history.end)
}

fn retain_search_chunk_ownership(results: &mut Vec<SearchResult>, owned_end: StableRowIndex) {
    // Overlap supplies trailing context only. A result belongs to the unique
    // chunk containing its start row, independent of backend-local match IDs.
    results.retain(|result| result.start_y < owned_end);
}
const SEARCH_RESULT_REQUEST_LIMIT_PER_CHUNK: u32 = 100_000;
const MAX_SEARCH_RESULTS_PER_CHUNK: usize = 100_000;
const MAX_EXPANDED_SEARCH_ROWS_PER_CHUNK: usize = 200_000;
const MAX_TOTAL_SEARCH_RESULTS: usize = 100_000;
const MAX_TOTAL_EXPANDED_SEARCH_ROWS: usize = 200_000;
const PARALLEL_SORT_MIN_RESULTS: usize = 4096;
const SEARCH_RETRY_BASE_MILLIS: u64 = 50;
const SEARCH_RETRY_MAX_MILLIS: u64 = 1000;
const MAX_SEARCH_RETRIES: u8 = 4;

pub struct CopyOverlay {
    delegate: Arc<dyn Pane>,
    render: Arc<Mutex<CopyRenderable>>,
    writer: Mutex<SearchOverlayPatternWriter>,
}

type CopyCoordinateSource = (
    crate::selection::SelectionAuthority,
    SequenceNo,
    RenderableDimensions,
);

/// Cursor, selection start and viewport are one coordinate transaction. The
/// token belongs to the terminal backend, never to numeric row arithmetic.
struct CopyCoordinateAnchor {
    source: CopyCoordinateSource,
    original: CopyCoordinateSource,
    points: [Option<wezterm_term::screen::SelectionAnchorCoordinate>; 3],
    state: [Option<mux::pane::PaneSelectionAnchor>; 3],
    captured: [bool; 3],
    deadline: std::time::Instant,
}

impl CopyCoordinateAnchor {
    fn allows_navigation(&self, current: Option<crate::selection::SelectionAuthority>) -> bool {
        current == Some(self.source.0)
    }

    fn failure_invalidates(
        &self,
        error: mux::pane::PaneSelectionAnchorError,
        current: Option<crate::selection::SelectionAuthority>,
    ) -> bool {
        error != mux::pane::PaneSelectionAnchorError::Unsupported
            || !self.allows_navigation(current)
    }

    fn poll(
        &mut self,
        pane: &dyn Pane,
    ) -> Result<
        Option<(
            [Option<wezterm_term::screen::SelectionAnchorCoordinate>; 3],
            CopyCoordinateSource,
        )>,
        mux::pane::PaneSelectionAnchorError,
    > {
        let result = self.poll_points(pane);
        if matches!(&result, Err(error) if *error != mux::pane::PaneSelectionAnchorError::Busy) {
            // A partially acquired set must not occupy the backend's bounded
            // token registry after this coordinate transaction has failed.
            self.state = std::array::from_fn(|_| None);
            self.captured = [false; 3];
        }
        result
    }

    fn poll_points(
        &mut self,
        pane: &dyn Pane,
    ) -> Result<
        Option<(
            [Option<wezterm_term::screen::SelectionAnchorCoordinate>; 3],
            CopyCoordinateSource,
        )>,
        mux::pane::PaneSelectionAnchorError,
    > {
        use mux::pane::{PaneSelectionAnchorError as Error, PaneSelectionAnchorStatus as Status};
        if std::time::Instant::now() >= self.deadline {
            return Err(Error::SourceChanged);
        }
        let mut ready = true;
        for (index, point) in self.points.iter().copied().enumerate() {
            let Some(point) = point else { continue };
            if self.captured[index] {
                continue;
            }
            // Independent coordinates are origins, never inclusive range ends.
            // BeforeZero cannot be an origin; encode that one start boundary as
            // a minimal range ending at cell zero and resolve only its start.
            let capture_points = if point.column.is_some() {
                [Some(point), None, None]
            } else {
                [
                    None,
                    Some(point),
                    Some(wezterm_term::screen::SelectionAnchorCoordinate {
                        column: Some(0),
                        row: point.row,
                    }),
                ]
            };
            match pane.capture_selection_anchor_capability(
                self.original.1,
                self.original.2,
                capture_points,
                &mut self.state[index],
            ) {
                Ok(Status::Pending) | Err(Error::Busy) => ready = false,
                Ok(Status::Captured) => self.captured[index] = true,
                Err(error) => return Err(error),
            }
        }
        if !ready {
            return Ok(None);
        }
        let before =
            crate::selection::SelectionAuthority::capture_source(pane).ok_or(Error::Busy)?;
        let mut points = [None; 3];
        let mut observed = None;
        for (index, original) in self.points.iter().copied().enumerate() {
            let Some(original) = original else { continue };
            match pane.selection_anchor_capability_snapshot(
                self.state[index].as_ref().ok_or(Error::SourceChanged)?,
            ) {
                Ok(Some((floor, sequence, dimensions, resolved))) => {
                    let resolved = resolved.ok_or(Error::SourceChanged)?;
                    points[index] = Some(
                        resolved[usize::from(original.column.is_none())]
                            .ok_or(Error::SourceChanged)?,
                    );
                    if observed.is_some_and(|previous| previous != (floor, sequence, dimensions)) {
                        ready = false;
                    }
                    observed = Some((floor, sequence, dimensions));
                }
                Ok(None) | Err(Error::Busy) => ready = false,
                Err(error) => return Err(error),
            }
        }
        if !ready {
            return Ok(None);
        }
        let (_, sequence, dimensions) = observed.ok_or(Error::SourceChanged)?;
        let source =
            crate::selection::SelectionAuthority::capture_source(pane).ok_or(Error::Busy)?;
        if source != before || source.1 != sequence || source.2 != dimensions {
            return Ok(None);
        }
        Ok(Some((points, source)))
    }
}

fn copy_resize_restarts_search(resized: bool, pattern_empty: bool) -> bool {
    resized && !pattern_empty
}

struct CopyCoordinateDriverGuard(Arc<AtomicBool>);

impl Drop for CopyCoordinateDriverGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

async fn coordinate_reply_before_deadline(
    reply: oneshot::Receiver<bool>,
    deadline: std::time::Instant,
) -> bool {
    if std::time::Instant::now() >= deadline {
        return false;
    }
    let timeout = sleep(deadline.saturating_duration_since(std::time::Instant::now()));
    futures::pin_mut!(reply, timeout);
    matches!(
        futures::future::select(reply, timeout).await,
        futures::future::Either::Left((Ok(true), _))
    )
}

fn close_copy_overlay_if_current(
    term_window: &TermWindow,
    pane_id: PaneId,
    instance_token: &Arc<()>,
) {
    let removed = {
        let Some(mut state) = term_window.pane_state(pane_id) else {
            return;
        };
        let is_current = state
            .overlay
            .as_ref()
            .and_then(|overlay| overlay.pane.downcast_ref::<CopyOverlay>())
            .is_some_and(|copy_overlay| {
                Arc::ptr_eq(&copy_overlay.render.lock().instance_token, instance_token)
            });
        if is_current {
            state.overlay.take();
            true
        } else {
            false
        }
    };
    if removed {
        if let Some(window) = term_window.window.as_ref() {
            window.invalidate();
        }
    }
}

#[derive(Copy, Clone, Debug)]
struct PendingJump {
    forward: bool,
    prev_char: bool,
}

#[derive(Copy, Clone, Debug)]
struct Jump {
    forward: bool,
    prev_char: bool,
    target: char,
}

type ContentPlans = anyhow::Result<Vec<wezterm_term::screen::ScreenLineRead>>;
type ContentEffect = Box<dyn FnOnce(&mut TermWindow) -> Result<bool, &'static str>>;

struct ContentReadReady {
    plans: Option<ContentPlans>,
    endpoint: Option<usize>,
    retire: std::sync::mpsc::SyncSender<ContentPlans>,
}

impl Drop for ContentReadReady {
    fn drop(&mut self) {
        if let Some(plans) = self.plans.take() {
            let _ = self.retire.send(plans);
        }
    }
}

/// Dimensions are reusable only during a synchronous, validated publication.
#[derive(Default)]
struct ContentViewportPublication {
    dimensions: Mutex<Option<RenderableDimensions>>,
}

impl ContentViewportPublication {
    fn with_dimensions(&self, dims: RenderableDimensions, publish: impl FnOnce()) {
        struct Clear<'a>(&'a ContentViewportPublication);
        impl Drop for Clear<'_> {
            fn drop(&mut self) {
                *self.0.dimensions.lock() = None;
            }
        }
        *self.dimensions.lock() = Some(dims);
        let _clear = Clear(self);
        publish();
    }

    fn update_dirty(
        &self,
        viewport: Option<StableRowIndex>,
        dirty: &mut RangeSet<StableRowIndex>,
    ) -> bool {
        let Some(dims) = *self.dimensions.lock() else {
            return false;
        };
        dirty.add(compute_search_row_from_viewport(viewport, dims));
        prune_dirty_results(dirty, retained_row_range(dims));
        true
    }
}

/// One accepted native navigation action. Hydrated storage remains charged to
/// its existing line-reader permit until the worker retires it.
struct ContentNavigation {
    token: Arc<()>,
    cancelled: Arc<AtomicBool>,
    deadline: std::time::Instant,
    cursor: (usize, StableRowIndex),
    start: Option<SelectionCoordinate>,
    mode: SelectionMode,
    end: bool,
    source: Option<(
        crate::selection::SelectionAuthority,
        SequenceNo,
        RenderableDimensions,
    )>,
    receiver: Option<std::sync::mpsc::Receiver<ContentReadReady>>,
    ready: Option<ContentReadReady>,
}

impl Drop for ContentNavigation {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

impl ContentNavigation {
    fn new(
        cursor: (usize, StableRowIndex),
        start: Option<SelectionCoordinate>,
        mode: SelectionMode,
        end: bool,
    ) -> Self {
        Self {
            token: Arc::new(()),
            cancelled: Arc::new(AtomicBool::new(false)),
            deadline: std::time::Instant::now() + Duration::from_secs(5),
            cursor,
            start,
            mode,
            end,
            source: None,
            receiver: None,
            ready: None,
        }
    }

    fn matches(
        &self,
        cursor: (usize, StableRowIndex),
        start: Option<SelectionCoordinate>,
        mode: SelectionMode,
    ) -> bool {
        !self.cancelled.load(Ordering::Acquire)
            && self.cursor == cursor
            && self.start == start
            && self.mode == mode
    }

    fn poll(
        &mut self,
        pane: &dyn Pane,
        cursor: (usize, StableRowIndex),
        start: Option<SelectionCoordinate>,
        mode: SelectionMode,
        publish: &mut dyn FnMut(usize, crate::selection::SelectionAuthority, RenderableDimensions),
    ) -> Result<bool, &'static str> {
        if !self.matches(cursor, start, mode) {
            return Err("copy content action superseded");
        }
        if self.cancelled.load(Ordering::Acquire) || std::time::Instant::now() >= self.deadline {
            return Err("copy content navigation cancelled or timed out");
        }
        let Some(range) = one_line_range(self.cursor.1) else {
            return Err("copy content row overflow");
        };
        if self.source.is_none() {
            // Only called for LocalPane: this is an atomic try-lock observation.
            self.source = crate::selection::SelectionAuthority::capture_source(pane);
        }
        let Some((authority, sequence, dimensions)) = self.source else {
            return Ok(false);
        };
        let Some((_, current_sequence, _)) =
            crate::selection::SelectionAuthority::capture_source(pane)
        else {
            return Ok(false);
        };
        if current_sequence != sequence {
            return Err("copy content source changed");
        }
        if self.receiver.is_none() {
            let Some(permit) = mux::pane::LineReadPermit::try_acquire() else {
                return Ok(false);
            };
            let cancel = Arc::clone(&self.cancelled);
            let deadline = self.deadline;
            let (sender, receiver) = std::sync::mpsc::sync_channel(1);
            let requested = range.clone();
            let end = self.end;
            let worker = permit
                .start(
                    move || cancel.load(Ordering::Acquire) || std::time::Instant::now() >= deadline,
                    move |plans, permit| {
                        let endpoint = plans.as_ref().ok().and_then(|plans| {
                            if plans.len() != 1 {
                                return None;
                            }
                            let mut bytes = wezterm_term::screen::ScreenLineRead::MAX_PAYLOAD_BYTES;
                            let mut work = 65_536;
                            let (top, rows) = plans[0].try_clone_viewport_for_snapshot(
                                requested.clone(),
                                &mut bytes,
                                &mut work,
                            )?;
                            if top != requested.start || rows.len() != 1 {
                                return None;
                            }
                            let mut x = 0;
                            for cell in rows[0].visible_cells() {
                                if cell.str() != " " {
                                    x = cell.cell_index();
                                    if !end {
                                        break;
                                    }
                                }
                            }
                            Some(x)
                        });
                        let (retire, retired) = std::sync::mpsc::sync_channel(1);
                        if sender
                            .send(ContentReadReady {
                                plans: Some(plans),
                                endpoint,
                                retire,
                            })
                            .is_ok()
                        {
                            drop(retired.recv());
                        }
                        drop(permit);
                    },
                )
                .map_err(|_| "copy content worker admission failed")?;
            match pane.capture_line_read(range.clone(), &mut Default::default()) {
                Some(Ok(plan)) => {
                    self.receiver = Some(receiver);
                    worker.submit(vec![plan.with_requested_physical_rows_only()]);
                    if crate::selection::SelectionAuthority::capture_source(pane)
                        .is_some_and(|(_, current, _)| current != sequence)
                    {
                        return Err("copy content changed during capture");
                    }
                }
                Some(Err(error)) if error.is::<wezterm_term::screen::ColdReadMetadataBusy>() => {
                    return Ok(false);
                }
                _ => return Err("copy content row could not be captured"),
            }
            return Ok(false);
        }
        if self.ready.is_none() {
            match self.receiver.as_ref().unwrap().try_recv() {
                Ok(ready) => self.ready = Some(ready),
                Err(std::sync::mpsc::TryRecvError::Empty) => return Ok(false),
                Err(_) => return Err("copy content worker stopped"),
            }
        }
        let plans = self
            .ready
            .as_ref()
            .unwrap()
            .plans
            .as_ref()
            .unwrap()
            .as_ref()
            .map_err(|_| "copy content hydration failed")?;
        if plans.len() != 1 {
            return Err("copy content read was incomplete");
        }
        let x = self
            .ready
            .as_ref()
            .unwrap()
            .endpoint
            .ok_or("copy content endpoint was incomplete")?;
        let local = pane
            .downcast_ref::<mux::localpane::LocalPane>()
            .ok_or("copy content navigation requires a native pane")?;
        let mut published_source = None;
        let valid = local.publish_line_reads_at_unchanged_coordinates(
            plans,
            authority.layout_floor(),
            sequence,
            dimensions,
            &mut |floor, sequence, dimensions| {
                if let Some(authority) = crate::selection::SelectionAuthority::from_native_snapshot(
                    pane, floor, dimensions,
                ) {
                    publish(x, authority, dimensions);
                    published_source = Some((authority, sequence, dimensions));
                }
            },
        );
        match valid {
            Err(_) => Ok(false),
            Ok(true) if published_source.is_some() => {
                self.source = published_source;
                Ok(true)
            }
            _ => Err("copy content source or layout changed"),
        }
    }
}

struct CopyRenderable {
    /// Exact, allocation-backed identity for this overlay instance. Numeric
    /// pane ids and per-instance run counters can both be reused; pointer
    /// identity cannot be forged by a later overlay occupying the same pane.
    instance_token: Arc<()>,
    cursor: StableCursorPosition,
    delegate: Arc<dyn Pane>,
    start: Option<SelectionCoordinate>,
    selection_mode: SelectionMode,
    viewport: Option<StableRowIndex>,
    /// We use this to cancel ourselves later
    window: ::window::Window,

    /// The text that the user entered
    pattern_type: PatternType,
    search_line: LineEditBuffer,
    /// The most recently queried set of matches
    results: Vec<SearchResult>,
    /// Source endpoint that authorized the corresponding entry in `results`.
    /// Copy-mode searches scrollback in chunks, so a single global endpoint
    /// cannot prove that every accumulated match is still current.
    result_source_ends: Vec<SequenceNo>,
    /// Exact searched chunk that authorized each corresponding result. Match
    /// validity can depend on multiline/regex context outside its own span.
    result_source_ranges: Vec<Range<StableRowIndex>>,
    expanded_result_rows: usize,
    by_line: HashMap<StableRowIndex, Vec<MatchResult>>,
    last_result_seqno: SequenceNo,
    last_bar_pos: Option<StableRowIndex>,
    dirty_results: RangeSet<StableRowIndex>,
    width: usize,
    height: usize,
    editing_search: bool,
    result_pos: Option<usize>,
    tab_id: TabId,
    /// Used to debounce queries while the user is typing
    typing_cookie: usize,
    searching: Option<Searching>,
    search_failed: Option<SearchRunIdentity>,
    next_search_run_id: usize,
    search_abort: Option<AbortHandle>,
    search_preparation_cancel: Option<Arc<AtomicBool>>,
    search_preparation_gate: Arc<Mutex<()>>,
    debounce_abort: Option<AbortHandle>,
    debounce_token: Option<Arc<()>>,
    retry_abort: Option<AbortHandle>,
    retry_token: Option<Arc<()>>,
    desired_result: Option<SearchResultAnchor>,
    desired_result_ordinal: Option<usize>,
    pending_jump: Option<PendingJump>,
    last_jump: Option<Jump>,
    content_navigation: Option<ContentNavigation>,
    content_action: Arc<()>,
    content_viewport_publication: Arc<ContentViewportPublication>,
    coordinate_anchor: Option<CopyCoordinateAnchor>,
    coordinate_driver: Option<promise::spawn::MainThreadSpawnedTask<()>>,
    coordinate_action: Arc<()>,
    deferred_navigation: VecDeque<KeyAssignment>,
    coordinates_invalid: bool,
    coordinate_driver_running: Arc<AtomicBool>,
    coordinate_remap_deadline: Option<std::time::Instant>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Searching {
    remain: StableRowIndex,
    run: SearchRunIdentity,
    range: Range<StableRowIndex>,
    retry_attempt: u8,
}

fn finish_exhausted_search(
    searching: &mut Option<Searching>,
    failed: &mut Option<SearchRunIdentity>,
    run: SearchRunIdentity,
) -> bool {
    if !searching
        .as_ref()
        .is_some_and(|pending| pending.run == run && pending.retry_attempt >= MAX_SEARCH_RETRIES)
    {
        return false;
    }
    *failed = Some(run);
    searching.take();
    true
}

fn admit_search_restart(failed: &mut Option<SearchRunIdentity>, preserve_result: bool) -> bool {
    if preserve_result && failed.is_some() {
        return false;
    }
    *failed = None;
    true
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SearchResultAnchor {
    start_x: usize,
    start_y: StableRowIndex,
    end_x: usize,
    end_y: StableRowIndex,
    match_id: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SearchRunIdentity {
    id: usize,
    source_seqno: SequenceNo,
    cols: usize,
    viewport_rows: usize,
    scrollback_rows: usize,
    physical_top: StableRowIndex,
    scrollback_top: StableRowIndex,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SearchCompletionStatus {
    Accepted,
    Superseded,
    SourceChanged,
}

#[derive(Debug)]
struct MatchResult {
    range: Range<usize>,
    result_index: usize,
}

struct PreparedCopyChunk {
    results: Vec<SearchResult>,
    by_line: HashMap<StableRowIndex, Vec<MatchResult>>,
    dirty_rows: RangeSet<StableRowIndex>,
    expanded_rows: usize,
}

fn merge_search_row_matches(
    current: &mut HashMap<StableRowIndex, Vec<MatchResult>>,
    incoming: HashMap<StableRowIndex, Vec<MatchResult>>,
) {
    for (row, mut matches) in incoming {
        current.entry(row).or_default().append(&mut matches);
    }
}

struct Dimensions {
    vertical_gap: isize,
    dims: RenderableDimensions,
    top: StableRowIndex,
}

fn merge_dirty_results(
    lines: Range<StableRowIndex>,
    mut delegate_dirty: RangeSet<StableRowIndex>,
    overlay_dirty: &RangeSet<StableRowIndex>,
) -> RangeSet<StableRowIndex> {
    delegate_dirty.add_set(overlay_dirty);
    delegate_dirty.intersection_with_range(lines)
}

/// Transfer overlay damage to the renderer only at the exact source-fence
/// discovery boundary. Ordinary line reads are deliberately non-destructive:
/// they are used by selection, automation, and accessibility consumers and
/// are not evidence that a frame will be presented.
///
/// This is the narrowest DamageGeneration handoff available through `Pane`.
/// It is still a single-consumer handoff rather than a per-window GPU-present
/// acknowledgement; callers that need independent damage generations require
/// a wider renderer API.
fn take_dirty_results(
    lines: Range<StableRowIndex>,
    mut delegate_dirty: RangeSet<StableRowIndex>,
    overlay_dirty: &mut RangeSet<StableRowIndex>,
) -> RangeSet<StableRowIndex> {
    let claimed = overlay_dirty.intersection_with_range(lines.clone());
    delegate_dirty.add_set(&claimed);

    let mut retained = RangeSet::default();
    for range in overlay_dirty.iter() {
        if range.start < lines.start {
            retained.add_range(range.start..range.end.min(lines.start));
        }
        if range.end > lines.end {
            retained.add_range(range.start.max(lines.end)..range.end);
        }
    }
    *overlay_dirty = retained;
    delegate_dirty.intersection_with_range(lines)
}

fn prune_dirty_results(
    dirty: &mut RangeSet<StableRowIndex>,
    retained: Option<Range<StableRowIndex>>,
) {
    *dirty = retained
        .map(|range| dirty.intersection_with_range(range))
        .unwrap_or_default();
}

fn retained_row_range(dims: RenderableDimensions) -> Option<Range<StableRowIndex>> {
    checked_stable_row_end(dims.scrollback_top, dims.scrollback_rows)
        .map(|end| dims.scrollback_top..end)
}

fn compute_search_row_from_viewport(
    viewport: Option<StableRowIndex>,
    dims: RenderableDimensions,
) -> StableRowIndex {
    // `RangeSet::add(row)` represents a scalar as `row..row + 1`, so MAX is
    // not a representable dirty row even though it is a valid scalar.
    let max_dirty_row = StableRowIndex::MAX.saturating_sub(1);
    let top = viewport.unwrap_or(dims.physical_top).min(max_dirty_row);
    let Some(last_row_offset) = dims
        .viewport_rows
        .checked_sub(1)
        .and_then(|offset| StableRowIndex::try_from(offset).ok())
    else {
        // A zero-height or unrepresentably tall viewport has no trustworthy
        // bottom row. Keep the overlay anchored at the known-representable
        // top instead of wrapping or inventing a row outside the viewport.
        return top;
    };

    top.checked_add(last_row_offset)
        .filter(|row| *row <= max_dirty_row)
        .unwrap_or(top)
}

fn checked_stable_row_end(top: StableRowIndex, row_count: usize) -> Option<StableRowIndex> {
    let row_count = StableRowIndex::try_from(row_count).ok()?;
    top.checked_add(row_count)
}

fn checked_last_stable_row(top: StableRowIndex, row_count: usize) -> Option<StableRowIndex> {
    let last_offset = row_count.checked_sub(1)?;
    let last_offset = StableRowIndex::try_from(last_offset).ok()?;
    top.checked_add(last_offset)
}

fn checked_page_up_boundary(top: StableRowIndex, viewport_rows: usize) -> Option<StableRowIndex> {
    let viewport_rows = StableRowIndex::try_from(viewport_rows).ok()?;
    top.checked_sub(viewport_rows)
}

fn checked_page_down_boundary(top: StableRowIndex, viewport_rows: usize) -> Option<StableRowIndex> {
    checked_stable_row_end(top, viewport_rows)
}

fn one_line_range(row: StableRowIndex) -> Option<Range<StableRowIndex>> {
    row.checked_add(1).map(|end| row..end)
}

fn checked_fractional_row_delta(viewport_rows: usize, amount: f64) -> Option<StableRowIndex> {
    let viewport_rows = StableRowIndex::try_from(viewport_rows).ok()?;
    let scaled = viewport_rows as f64 * amount;
    let min_inclusive = StableRowIndex::MIN as f64;
    let max_exclusive = -min_inclusive;
    if !scaled.is_finite() || scaled < min_inclusive || scaled >= max_exclusive {
        return None;
    }
    Some(scaled.trunc() as StableRowIndex)
}

fn allocate_search_run_id(next: &mut usize) -> Option<usize> {
    let id = *next;
    *next = id.checked_add(1)?;
    Some(id)
}

fn search_retry_delay(attempt: u8) -> Duration {
    let shift = u32::from(attempt.min(10));
    let multiplier = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
    Duration::from_millis(
        SEARCH_RETRY_BASE_MILLIS
            .saturating_mul(multiplier)
            .min(SEARCH_RETRY_MAX_MILLIS),
    )
}

fn search_result_anchor(result: &SearchResult) -> SearchResultAnchor {
    SearchResultAnchor {
        start_x: result.start_x,
        start_y: result.start_y,
        end_x: result.end_x,
        end_y: result.end_y,
        match_id: result.match_id,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ValidatedSearchGeometry {
    start_x: usize,
    start_y: StableRowIndex,
    end_x: usize,
    end_y: StableRowIndex,
    row_span: usize,
}

fn validate_search_geometry(
    start_x: usize,
    start_y: StableRowIndex,
    end_x: usize,
    end_y: StableRowIndex,
    searched: &Range<StableRowIndex>,
    cols: usize,
) -> Option<ValidatedSearchGeometry> {
    if cols == 0
        || searched.start >= searched.end
        || start_y > end_y
        || start_y < searched.start
        || end_x > cols
        || start_x > cols
    {
        return None;
    }
    let searched_last = searched.end.checked_sub(1)?;
    if end_y > searched_last {
        return None;
    }

    let row_span = end_y
        .checked_sub(start_y)?
        .checked_add(1)
        .and_then(|span| usize::try_from(span).ok())?;
    if (start_y == end_y && start_x >= end_x) || (row_span == 2 && start_x == cols && end_x == 0) {
        return None;
    }

    Some(ValidatedSearchGeometry {
        start_x,
        start_y,
        end_x,
        end_y,
        row_span,
    })
}

fn inclusive_search_result_end(
    result: &SearchResult,
    cols: usize,
) -> Option<(usize, StableRowIndex)> {
    if result.end_x > 0 {
        Some((result.end_x.checked_sub(1)?, result.end_y))
    } else if result.end_y > result.start_y && cols > 0 {
        Some((cols.checked_sub(1)?, result.end_y.checked_sub(1)?))
    } else {
        None
    }
}

fn sanitize_search_results(
    results: Vec<SearchResult>,
    searched: &Range<StableRowIndex>,
    cols: usize,
    cancel: &AtomicBool,
) -> Option<Vec<SearchResult>> {
    let mut sanitized = Vec::with_capacity(results.len().min(MAX_SEARCH_RESULTS_PER_CHUNK));
    let mut expanded_rows = 0usize;
    for (index, mut result) in results
        .into_iter()
        .take(MAX_SEARCH_RESULTS_PER_CHUNK)
        .enumerate()
    {
        if index.is_multiple_of(256) && cancel.load(Ordering::Relaxed) {
            return None;
        }
        let Some(geometry) = validate_search_geometry(
            result.start_x,
            result.start_y,
            result.end_x,
            result.end_y,
            searched,
            cols,
        ) else {
            continue;
        };
        let Some(next_expanded_rows) = expanded_rows.checked_add(geometry.row_span) else {
            break;
        };
        if next_expanded_rows > MAX_EXPANDED_SEARCH_ROWS_PER_CHUNK {
            break;
        }
        expanded_rows = next_expanded_rows;
        result.start_x = geometry.start_x;
        result.start_y = geometry.start_y;
        result.end_x = geometry.end_x;
        result.end_y = geometry.end_y;
        sanitized.push(result);
    }
    (!cancel.load(Ordering::Relaxed)).then_some(sanitized)
}

/// Perform all sorting, geometry validation, and per-row expansion before
/// entering the window `Apply` callback. Installing this prepared chunk only moves bounded
/// collections and merges at most one entry per searched row while the UI
/// renderer lock is held.
fn prepare_copy_search_chunk(
    results: Vec<SearchResult>,
    searched: &Range<StableRowIndex>,
    cols: usize,
    result_base: usize,
    result_capacity: usize,
    expanded_row_capacity: usize,
    cancel: &AtomicBool,
) -> Option<PreparedCopyChunk> {
    let mut results = sanitize_search_results(results, searched, cols, cancel)?;
    if cancel.load(Ordering::Relaxed) {
        return None;
    }
    if results.len() >= PARALLEL_SORT_MIN_RESULTS {
        results.par_sort_unstable();
    } else {
        results.sort_unstable();
    }
    if cancel.load(Ordering::Relaxed) {
        return None;
    }
    results.reverse();

    let mut prepared_results = Vec::with_capacity(results.len().min(result_capacity));
    let mut by_line: HashMap<StableRowIndex, Vec<MatchResult>> = HashMap::new();
    let mut dirty_rows = RangeSet::default();
    let mut expanded_rows = 0usize;

    for (index, result) in results.into_iter().take(result_capacity).enumerate() {
        if index.is_multiple_of(256) && cancel.load(Ordering::Relaxed) {
            return None;
        }
        let Some(row_span) = result
            .end_y
            .checked_sub(result.start_y)
            .and_then(|span| span.checked_add(1))
            .and_then(|span| usize::try_from(span).ok())
        else {
            continue;
        };
        let Some(next_expanded_rows) = expanded_rows.checked_add(row_span) else {
            break;
        };
        if next_expanded_rows > expanded_row_capacity {
            break;
        }
        let Some(result_index) = result_base.checked_add(prepared_results.len()) else {
            break;
        };

        for (row_offset, row) in (result.start_y..=result.end_y).enumerate() {
            if row_offset.is_multiple_of(256) && cancel.load(Ordering::Relaxed) {
                return None;
            }
            let range = if row == result.start_y && row == result.end_y {
                result.start_x..result.end_x
            } else if row == result.end_y {
                0..result.end_x
            } else if row == result.start_y {
                result.start_x..cols
            } else {
                0..cols
            };
            if range.start >= range.end {
                continue;
            }
            by_line.entry(row).or_default().push(MatchResult {
                range,
                result_index,
            });
            dirty_rows.add(row);
        }

        expanded_rows = next_expanded_rows;
        prepared_results.push(result);
    }

    Some(PreparedCopyChunk {
        results: prepared_results,
        by_line,
        dirty_rows,
        expanded_rows,
    })
}

async fn prepare_copy_search_chunk_off_thread(
    mut results: Vec<SearchResult>,
    searched: Range<StableRowIndex>,
    owned_end: StableRowIndex,
    cols: usize,
    result_base: usize,
    result_capacity: usize,
    expanded_row_capacity: usize,
    cancel: Arc<AtomicBool>,
    gate: Arc<Mutex<()>>,
) -> Result<PreparedCopyChunk, String> {
    let (sender, receiver) = oneshot::channel();
    rayon::spawn(move || {
        let Some(_gate_guard) = gate.try_lock() else {
            let _ = sender.send(Err("copy search preparation lane busy".to_string()));
            return;
        };
        retain_search_chunk_ownership(&mut results, owned_end);
        let prepared = prepare_copy_search_chunk(
            results,
            &searched,
            cols,
            result_base,
            result_capacity,
            expanded_row_capacity,
            &cancel,
        );
        if let Some(prepared) = prepared {
            let _ = sender.send(Ok(prepared));
        } else {
            let _ = sender.send(Err("copy search preparation cancelled".to_string()));
        }
    });
    receiver
        .await
        .map_err(|_| "copy search result preparation worker stopped".to_string())?
}

fn search_run_identity(
    id: usize,
    source_seqno: SequenceNo,
    dims: RenderableDimensions,
) -> SearchRunIdentity {
    SearchRunIdentity {
        id,
        source_seqno,
        cols: dims.cols,
        viewport_rows: dims.viewport_rows,
        scrollback_rows: dims.scrollback_rows,
        physical_top: dims.physical_top,
        scrollback_top: dims.scrollback_top,
    }
}

fn classify_search_completion(
    pending: Option<&Searching>,
    run: SearchRunIdentity,
    range: &Range<StableRowIndex>,
    current_source: SearchRunIdentity,
    source_range_changed: bool,
) -> SearchCompletionStatus {
    let Some(pending) = pending else {
        return SearchCompletionStatus::Superseded;
    };
    if pending.run != run || pending.range != *range {
        return SearchCompletionStatus::Superseded;
    }
    let current_retained_end = checked_stable_row_end(
        current_source.scrollback_top,
        current_source.scrollback_rows,
    );
    if run.cols != current_source.cols
        || current_source.scrollback_top > range.start
        || current_retained_end.is_none_or(|end| range.end > end)
        || source_range_changed
    {
        return SearchCompletionStatus::SourceChanged;
    }
    SearchCompletionStatus::Accepted
}

fn previous_row_within_scrollback(
    row: StableRowIndex,
    scrollback_top: StableRowIndex,
) -> Option<StableRowIndex> {
    if row > scrollback_top {
        row.checked_sub(1)
    } else {
        None
    }
}

#[cfg(test)]
mod dirty_tracking_tests {
    use super::*;

    #[test]
    fn content_viewport_dimensions_only_apply_during_validated_publication() {
        let publication = ContentViewportPublication::default();
        let mut dirty = RangeSet::default();
        dirty.add(150);
        // An outstanding command, including one with stale captured dimensions,
        // cannot supply the ordinary viewport callback's pruning authority.
        assert!(!publication.update_dirty(Some(0), &mut dirty));
        assert_eq!(collect_ranges(&dirty), vec![150..151]);
        publication.with_dimensions(dimensions(90, 10), || {
            assert!(publication.update_dirty(Some(90), &mut dirty));
            assert_eq!(collect_ranges(&dirty), vec![99..100]);
        });
        dirty.add(150);
        assert!(!publication.update_dirty(Some(140), &mut dirty));
        assert_eq!(collect_ranges(&dirty), vec![99..100, 150..151]);

        // Unwinding a synchronous viewport callback must not leak its old
        // dimensions into a later ordinary viewport notification.
        let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            publication.with_dimensions(dimensions(90, 10), || {
                panic!("test viewport publication unwind");
            });
        }));
        assert!(failed.is_err());
        assert!(!publication.update_dirty(Some(140), &mut dirty));
        assert_eq!(collect_ranges(&dirty), vec![99..100, 150..151]);
    }

    #[test]
    #[cfg(unix)]
    fn native_content_navigation_cold_unicode_busy_and_stale_actions() {
        use wezterm_term::config::{ScrollbackSpillSink, ScrollbackTierConfig};
        // SimpleExecutor installs a process-global scheduler. Isolate its
        // lifetime from parallel frontend tests that replace that scheduler.
        const CHILD: &str = "FT_COPY_NAVIGATION_TEST_CHILD";
        const NAME: &str = "overlay::copy::dirty_tracking_tests::native_content_navigation_cold_unicode_busy_and_stale_actions";
        if std::env::var(CHILD).as_deref() != Ok(NAME) {
            let mut logs = tempfile::tempdir().unwrap();
            logs.disable_cleanup(true);
            let stdout_path = logs.path().join("stdout.log");
            let stderr_path = logs.path().join("stderr.log");
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([NAME, "--exact", "--nocapture"])
                .env(CHILD, NAME)
                .stdin(std::process::Stdio::null())
                .stdout(std::fs::File::create(&stdout_path).unwrap())
                .stderr(std::fs::File::create(&stderr_path).unwrap())
                .spawn()
                .unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(60);
            let status = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break Ok(status),
                    Ok(None) if std::time::Instant::now() < deadline => {}
                    other => break Err(format!("child did not complete: {other:?}")),
                }
                std::thread::sleep(Duration::from_millis(5));
            };
            let cleanup = status.is_err().then(|| (child.kill(), child.wait()));
            let stdout = std::fs::read_to_string(stdout_path).unwrap();
            let stderr = std::fs::read_to_string(stderr_path).unwrap();
            assert!(
                status.as_ref().is_ok_and(|status| status.success())
                    && stdout.contains(&format!("test {NAME} ... ok"))
                    && stdout.contains("1 passed; 0 failed"),
                "owned navigation child failed: {status:?}; cleanup={cleanup:?}\n{stdout}\n{stderr}"
            );
            return;
        }
        let executor = promise::spawn::SimpleExecutor::try_with_limits(
            promise::spawn::MainThreadAdmissionLimits::new(32, 1024 * 1024, 0, 0).unwrap(),
        )
        .unwrap();
        #[derive(Debug)]
        struct ColdConfig(Arc<dyn ScrollbackSpillSink>);
        impl wezterm_term::TerminalConfiguration for ColdConfig {
            fn color_palette(&self) -> ColorPalette {
                Default::default()
            }
            fn scrollback_size(&self) -> usize {
                4096
            }
            fn scrollback_tier_config(&self) -> ScrollbackTierConfig {
                ScrollbackTierConfig {
                    enabled: true,
                    hot_lines: 1,
                    warm_max_bytes: 0,
                }
            }
            fn scrollback_spill_sink(&self) -> Option<Arc<dyn ScrollbackSpillSink>> {
                Some(Arc::clone(&self.0))
            }
        }
        let mut root = tempfile::tempdir().unwrap();
        root.disable_cleanup(true);
        let sink = frankenterm_mux_server_impl::open_scrollback_spill_sink(
            root.path().to_path_buf(),
            &config::ScrollbackSpillSinkContext {
                pane_id: 998_731,
                domain_id: 998_731,
                durable_pane_id: *uuid::Uuid::new_v4().as_bytes(),
                command_description: "owned content navigation regression".to_owned(),
            },
        )
        .unwrap();
        let mut terminal = wezterm_term::Terminal::new(
            TerminalSize {
                rows: 3,
                cols: 16,
                dpi: 96,
                pixel_width: 128,
                pixel_height: 48,
            },
            Arc::new(ColdConfig(Arc::clone(&sink))),
            "content-navigation",
            "test",
            Box::new(Vec::<u8>::new()),
        );
        terminal.advance_bytes("  界e\u{301}  Z \r\n".as_bytes());
        for _ in 0..1024 {
            terminal.advance_bytes(b"filler\r\n");
        }
        sink.flush_scrollback().unwrap();
        assert!(sink.retained_scrollback_rows() > 256);
        assert!(terminal.screen().in_memory_scrollback_rows() < 8);
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize::default())
            .unwrap();
        let writer = pair.master.take_writer().unwrap();
        let child = pair
            .slave
            .spawn_command(portable_pty::CommandBuilder::new("/bin/cat"))
            .unwrap();
        let pane: Arc<dyn Pane> = Arc::new(mux::localpane::LocalPane::new(
            998_731,
            terminal,
            child,
            pair.master,
            writer,
            998_731,
            [0x73; 16],
            "owned navigation".to_owned(),
        ));
        struct ChildGuard(Arc<dyn Pane>);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                self.0.kill();
                let deadline = std::time::Instant::now() + Duration::from_secs(3);
                while !self.0.is_dead() && std::time::Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(5));
                }
                assert!(self.0.is_dead(), "owned navigation child must be reaped");
            }
        }
        let _child = ChildGuard(Arc::clone(&pane));
        // Cold anchor capture/resolve publishes through the exact live mux
        // registration. An unregistered LocalPane cannot admit that work,
        // even though the direct owned content-navigation read can complete.
        // Keep a real private owner alive without replacing the global mux or
        // the shared GUI executor used by other tests.
        let owner = Arc::new(mux::Mux::new(None));
        owner.add_pane(&pane).unwrap();
        assert!(owner.capture_pane_registration(&pane).is_some());
        // Check the cold bytes with the synchronous oracle. This does not
        // publish an owned layout: the first action must handle that itself.
        let (top, rows) = pane.get_lines(0..1);
        assert_eq!(top, 0);
        assert_eq!(rows[0].as_str().trim_end(), "  界e\u{301}  Z");
        let bottom = pane.get_dimensions().physical_top;
        struct Hold {
            entered: std::sync::mpsc::SyncSender<()>,
            release: std::sync::mpsc::Receiver<()>,
        }
        impl WithPaneLines for Hold {
            fn with_lines_mut(&mut self, _: StableRowIndex, _: &mut [&mut Line]) {
                self.entered.send(()).unwrap();
                self.release.recv_timeout(Duration::from_secs(3)).unwrap();
            }
        }
        for (end, expected) in [(false, 2), (true, 7)] {
            let mut action = ContentNavigation::new((0, 0), None, SelectionMode::Cell, end);
            let (entered, held) = std::sync::mpsc::sync_channel(1);
            let (release, wait) = std::sync::mpsc::sync_channel(1);
            let owned = Arc::clone(&pane);
            let holder = std::thread::spawn(move || {
                owned.with_lines_mut(
                    bottom..bottom + 1,
                    &mut Hold {
                        entered,
                        release: wait,
                    },
                )
            });
            held.recv_timeout(Duration::from_secs(3)).unwrap();
            let began = std::time::Instant::now();
            let mut result = None;
            assert!(
                !action
                    .poll(&*pane, (0, 0), None, SelectionMode::Cell, &mut |x, _, _| {
                        result = Some(x)
                    })
                    .unwrap()
            );
            assert!(
                began.elapsed() < Duration::from_secs(1),
                "accepted action blocked on held terminal"
            );
            assert_eq!(result, None);
            release.send(()).unwrap();
            holder.join().unwrap();
            loop {
                if action
                    .poll(&*pane, (0, 0), None, SelectionMode::Cell, &mut |x, _, _| {
                        result = Some(x)
                    })
                    .unwrap()
                {
                    break;
                }
                assert!(std::time::Instant::now() < action.deadline);
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(result, Some(expected), "exact wide/combining-cell endpoint");
        }
        let source = crate::selection::SelectionAuthority::capture_source(&*pane).unwrap();
        let (entered, held) = std::sync::mpsc::sync_channel(1);
        let (release, wait) = std::sync::mpsc::sync_channel(1);
        let owned = Arc::clone(&pane);
        let holder = std::thread::spawn(move || {
            owned.with_lines_mut(
                bottom..bottom + 1,
                &mut Hold {
                    entered,
                    release: wait,
                },
            )
        });
        held.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(
            crate::termwindow::render::pane::ViewportAnchor::new(&*pane, 0, source.2).is_none()
        );
        let mut viewport_anchor =
            crate::termwindow::render::pane::ViewportAnchor::from_source(source, 0);
        assert!(matches!(
            viewport_anchor.poll(&*pane),
            Err(mux::pane::PaneSelectionAnchorError::Busy)
        ));
        release.send(()).unwrap();
        holder.join().unwrap();
        let viewport_deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            executor.try_tick().unwrap();
            match viewport_anchor.poll(&*pane) {
                Ok(Some((row, _))) => {
                    assert_eq!(row, 0);
                    break;
                }
                Ok(None) | Err(mux::pane::PaneSelectionAnchorError::Busy) => {}
                other => panic!("real viewport capture failed: {other:?}"),
            }
            assert!(std::time::Instant::now() < viewport_deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut coordinates = CopyCoordinateAnchor {
            source,
            original: source,
            points: [
                Some(wezterm_term::screen::SelectionAnchorCoordinate {
                    column: Some(7),
                    row: 0,
                }),
                Some(wezterm_term::screen::SelectionAnchorCoordinate {
                    column: Some(2),
                    row: 0,
                }),
                Some(wezterm_term::screen::SelectionAnchorCoordinate {
                    column: Some(0),
                    row: 0,
                }),
            ],
            state: std::array::from_fn(|_| None),
            captured: [false; 3],
            deadline: std::time::Instant::now() + Duration::from_secs(5),
        };
        // Unsupported peers still permit ordinary navigation in the exact
        // original frame: token presence is not fabricated as a prerequisite.
        assert!(coordinates.allows_navigation(Some(source.0)));
        assert!(
            !coordinates.failure_invalidates(
                mux::pane::PaneSelectionAnchorError::Unsupported,
                Some(source.0)
            ),
            "legacy unsupported capture preserves same-layout navigation"
        );
        loop {
            match coordinates.poll(&*pane) {
                Ok(Some(_)) => break,
                Ok(None) | Err(mux::pane::PaneSelectionAnchorError::Busy) => {}
                other => panic!("real cold coordinate capture failed: {other:?}"),
            }
            assert!(std::time::Instant::now() < coordinates.deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(coordinates.captured.iter().all(|captured| *captured));
        // Exercise the actual backend endpoint contract for every independent
        // copy-mode optional-coordinate shape, including ordinary navigation
        // before a selection start exists. Keep these tokens across real reflow.
        let mut optional_coordinates = Vec::new();
        for (case, (has_start, has_viewport)) in [
            (false, false),
            (false, true),
            (true, false),
            (true, true),
            (true, false),
            (false, true),
        ]
        .into_iter()
        .enumerate()
        {
            let mut points = coordinates.points;
            if !has_start {
                points[1] = None;
            }
            if !has_viewport {
                points[2] = None;
            }
            if case == 4 {
                points[1].as_mut().unwrap().column = None;
            }
            if case == 5 {
                // Reverse cursor/viewport order without changing point roles.
                points[0].as_mut().unwrap().column = Some(0);
                points[2].as_mut().unwrap().row = 1;
            }
            let mut anchor = CopyCoordinateAnchor {
                source,
                original: source,
                points,
                state: std::array::from_fn(|_| None),
                captured: [false; 3],
                deadline: std::time::Instant::now() + Duration::from_secs(5),
            };
            loop {
                executor.try_tick().unwrap();
                match anchor.poll(&*pane) {
                    Ok(Some((actual, _))) => {
                        assert_eq!(actual, points, "initial optional copy coordinates");
                        break;
                    }
                    Ok(None) | Err(mux::pane::PaneSelectionAnchorError::Busy) => {}
                    other => panic!("optional copy coordinate capture failed: {other:?}"),
                }
                assert!(std::time::Instant::now() < anchor.deadline);
                std::thread::sleep(Duration::from_millis(5));
            }
            // The original `coordinates` already exercises both-present
            // transport across reflow. Retire this duplicate before admitting
            // more cases; the fixture must not consume the entire registry.
            if case != 3 {
                optional_coordinates.push((case, anchor));
            }
        }
        for stale in ["cursor", "selection", "close", "source", "geometry"] {
            let mut action = ContentNavigation::new((0, 0), None, SelectionMode::Cell, true);
            while action.receiver.is_none() {
                assert!(
                    !action
                        .poll(
                            &*pane,
                            (0, 0),
                            None,
                            SelectionMode::Cell,
                            &mut |_, _, _| panic!("first capture must not publish")
                        )
                        .unwrap()
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            action.ready = Some(
                action
                    .receiver
                    .as_ref()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap(),
            );
            let mut cursor = (0, 0);
            let mut start = None;
            match stale {
                "cursor" => cursor.0 = 1,
                "selection" => start = Some(SelectionCoordinate::x_y(1, 0)),
                "close" => action.cancelled.store(true, Ordering::Release),
                "source" => {
                    let before = pane.get_current_seqno();
                    pane.perform_actions(vec![termwiz::escape::Action::PrintString(
                        "changed".to_owned(),
                    )])
                    .unwrap();
                    while pane.get_current_seqno() == before {
                        assert!(std::time::Instant::now() < action.deadline);
                        executor.try_tick().unwrap();
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
                "geometry" => {
                    pane.resize(TerminalSize {
                        rows: 3,
                        cols: 8,
                        dpi: 96,
                        pixel_width: 64,
                        pixel_height: 48,
                    })
                    .unwrap();
                    // resize() admits an asynchronous worker. Fence its real
                    // geometry publication before testing the stale read;
                    // admission alone still permits the old source.
                    loop {
                        executor.try_tick().unwrap();
                        assert!(
                            std::time::Instant::now() < action.deadline,
                            "owned resize must complete within the original action deadline"
                        );
                        if crate::selection::SelectionAuthority::capture_source(&*pane).is_some_and(
                            |(_, _, dims)| {
                                dims.cols == 8
                                    && dims.viewport_rows == 3
                                    && dims.dpi == 96
                                    && dims.pixel_width == 64
                                    && dims.pixel_height == 48
                            },
                        ) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
                _ => unreachable!(),
            }
            let mut published = false;
            assert!(
                action
                    .poll(
                        &*pane,
                        cursor,
                        start,
                        SelectionMode::Cell,
                        &mut |_, _, _| published = true
                    )
                    .is_err(),
                "{stale}"
            );
            assert!(!published, "stale {stale} must not move the cursor");
        }
        pane.resize(TerminalSize {
            rows: 3,
            cols: 4,
            dpi: 96,
            pixel_width: 32,
            pixel_height: 48,
        })
        .unwrap();
        coordinates.deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            executor.try_tick().unwrap();
            let current = crate::selection::SelectionAuthority::capture_source(&*pane);
            if current.is_some_and(|(_, _, dims)| dims.cols == 4) {
                break;
            }
            assert!(std::time::Instant::now() < coordinates.deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            !coordinates.allows_navigation(crate::selection::SelectionAuthority::capture(&*pane)),
            "even an unsupported anchor must not authorize old coordinates after resize"
        );
        assert!(
            coordinates.failure_invalidates(
                mux::pane::PaneSelectionAnchorError::Unsupported,
                crate::selection::SelectionAuthority::capture(&*pane)
            ),
            "legacy unsupported capture refuses changed-layout coordinates"
        );
        let mapped = loop {
            executor.try_tick().unwrap();
            match coordinates.poll(&*pane) {
                Ok(Some((points, _))) => break points,
                Ok(None) | Err(mux::pane::PaneSelectionAnchorError::Busy) => {}
                other => panic!("real cold coordinate resolution failed: {other:?}"),
            }
            assert!(std::time::Instant::now() < coordinates.deadline);
            std::thread::sleep(Duration::from_millis(5));
        };
        let start = mapped[1].unwrap();
        let end = mapped[0].unwrap();
        let viewport = mapped[2].unwrap();
        for (case, mut anchor) in optional_coordinates {
            anchor.deadline = std::time::Instant::now() + Duration::from_secs(5);
            let mut expected =
                std::array::from_fn(|index| anchor.points[index].and_then(|_| mapped[index]));
            if case == 4 {
                expected[1] = Some(wezterm_term::screen::SelectionAnchorCoordinate {
                    column: None,
                    row: viewport.row,
                });
            }
            loop {
                executor.try_tick().unwrap();
                match anchor.poll(&*pane) {
                    Ok(Some((actual, current))) => {
                        assert_eq!(current.2.cols, 4);
                        if case == 5 {
                            assert_eq!(actual[0], Some(viewport));
                            assert!(actual[1].is_none());
                            let view = actual[2].unwrap();
                            assert_eq!(view.column, Some(0));
                            assert!(view.row > actual[0].unwrap().row);
                            let read = pane
                                .capture_line_read(view.row..view.row + 1, &mut Default::default())
                                .unwrap()
                                .unwrap()
                                .hydrate(|| false)
                                .unwrap();
                            let (first, lines) = read
                                .try_clone_viewport_for_snapshot(
                                    view.row..view.row + 1,
                                    &mut (32 * 1024 * 1024),
                                    &mut 65_536,
                                )
                                .unwrap();
                            assert_eq!(first, view.row);
                            assert_eq!(
                                lines[0].columns_as_str(0..1),
                                "f",
                                "viewport retains original filler start after cursor crosses it"
                            );
                        } else {
                            assert_eq!(
                                actual, expected,
                                "remapped optional copy coordinates case {case}"
                            );
                        }
                        break;
                    }
                    Ok(None) | Err(mux::pane::PaneSelectionAnchorError::Busy) => {}
                    other => panic!("optional copy coordinate remap failed: {other:?}"),
                }
                assert!(std::time::Instant::now() < anchor.deadline);
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let mut uncaptured_old_viewport =
            crate::termwindow::render::pane::ViewportAnchor::from_source(source, 0);
        assert!(matches!(
            uncaptured_old_viewport.poll(&*pane),
            Err(mux::pane::PaneSelectionAnchorError::SourceChanged)
        ));
        let viewport_deadline = std::time::Instant::now() + Duration::from_secs(5);
        let remapped_viewport = loop {
            executor.try_tick().unwrap();
            match viewport_anchor.poll(&*pane) {
                Ok(Some((row, dimensions))) => {
                    assert_eq!(dimensions.cols, 4);
                    break row;
                }
                Ok(None) | Err(mux::pane::PaneSelectionAnchorError::Busy) => {}
                other => panic!("real viewport remap failed: {other:?}"),
            }
            assert!(std::time::Instant::now() < viewport_deadline);
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(remapped_viewport, viewport.row);
        assert_ne!(
            remapped_viewport, 0,
            "retaining the old numeric viewport would display different content"
        );
        // The bounded wrap planner may leave an underfull row containing the
        // leading spaces. Cell zero and cell two need not share a visual row.
        // Prove their original content offsets through the actual hydrated
        // rows, rather than imposing a greedy wrapping geometry.
        assert_eq!(viewport.column, Some(0));
        assert!(viewport.row <= start.row);
        assert!(
            end.row > start.row,
            "four-column resize must really reflow the selected prefix"
        );
        let requested = viewport.row..end.row + 1;
        let read = pane
            .capture_line_read(requested.clone(), &mut Default::default())
            .expect("native cold read capability")
            .expect("capture remapped cold rows")
            .hydrate(|| false)
            .expect("hydrate remapped cold rows");
        let (first, rows) = read
            .try_clone_viewport_for_snapshot(requested, &mut (32 * 1024 * 1024), &mut 65_536)
            .expect("complete bounded remapped cold rows");
        assert_eq!(first, viewport.row);
        let mut prefix = String::new();
        let mut logical_cells = 0;
        let mut selected = String::new();
        for (offset, line) in rows.iter().enumerate() {
            let row = first + offset as isize;
            if row < start.row {
                prefix.push_str(&line.columns_as_str(0..line.len()));
                logical_cells += line.len();
                continue;
            }
            if row == start.row {
                prefix.push_str(&line.columns_as_str(0..start.column.unwrap()));
                assert_eq!(logical_cells + start.column.unwrap(), 2);
            }
            if row == end.row {
                assert_eq!(logical_cells + end.column.unwrap(), 7);
            }
            let left = if row == start.row {
                start.column.unwrap()
            } else {
                0
            };
            let right = if row == end.row {
                end.column.unwrap() + 1
            } else {
                4
            };
            selected.push_str(&line.columns_as_str(left..right));
            logical_cells += line.len();
        }
        assert_eq!(prefix, "  ", "viewport retains the exact unselected prefix");
        assert_eq!(
            selected, "界e\u{301}  Z",
            "actual cold reflow retains exact original selected bytes"
        );
    }

    #[test]
    fn non_search_copy_resize_does_not_restart_selection_clearing_search() {
        assert!(!copy_resize_restarts_search(true, true));
        assert!(copy_resize_restarts_search(true, false));
        assert!(!copy_resize_restarts_search(false, false));
    }

    #[test]
    fn coordinate_driver_deadline_rejects_late_reply_and_releases_running_owner() {
        use futures::FutureExt;
        let running = Arc::new(AtomicBool::new(true));
        let guard = CopyCoordinateDriverGuard(Arc::clone(&running));
        let (send, recv) = oneshot::channel();
        send.send(true).unwrap();
        assert_eq!(
            coordinate_reply_before_deadline(recv, std::time::Instant::now()).now_or_never(),
            Some(false),
            "an already queued reply cannot authorize an expired action"
        );
        drop(guard);
        assert!(!running.load(Ordering::Acquire));
        let (send, recv) = oneshot::channel();
        send.send(true).unwrap();
        assert_eq!(
            coordinate_reply_before_deadline(
                recv,
                std::time::Instant::now() + Duration::from_secs(5)
            )
            .now_or_never(),
            Some(true)
        );
    }

    fn make_dirty_ranges(
        ranges: impl IntoIterator<Item = Range<StableRowIndex>>,
    ) -> RangeSet<StableRowIndex> {
        let mut dirty = RangeSet::default();
        for range in ranges {
            dirty.add_range(range);
        }
        dirty
    }

    fn collect_ranges(set: &RangeSet<StableRowIndex>) -> Vec<Range<StableRowIndex>> {
        set.iter().cloned().collect()
    }

    fn dimensions(physical_top: StableRowIndex, viewport_rows: usize) -> RenderableDimensions {
        RenderableDimensions {
            cols: 80,
            viewport_rows,
            scrollback_rows: 100,
            physical_top,
            scrollback_top: 0,
            dpi: 96,
            pixel_width: 640,
            pixel_height: 80,
            reverse_video: false,
        }
    }

    #[test]
    fn copy_search_overlap_preserves_boundary_match_once() {
        let history = 0..3000;
        let newer = 2000..3000;
        let older = older_search_chunk(newer.start, history.clone());
        assert_eq!(older, 1000..3000);
        let crossing = SearchResult {
            start_x: 79,
            start_y: 1999,
            end_x: 4,
            end_y: 2000,
            match_id: 0,
        };
        let duplicate = SearchResult {
            start_x: 8,
            start_y: 2000,
            end_x: 12,
            end_y: 2000,
            match_id: 0,
        };
        let cancel = AtomicBool::new(false);
        let newest =
            prepare_copy_search_chunk(vec![crossing, duplicate], &newer, 80, 0, 100, 200, &cancel)
                .unwrap();
        assert_eq!(newest.results, vec![duplicate]);
        let mut candidates = vec![crossing, duplicate];
        retain_search_chunk_ownership(&mut candidates, newer.start);
        let previous = prepare_copy_search_chunk(
            candidates,
            &older,
            80,
            newest.results.len(),
            100,
            200,
            &cancel,
        )
        .unwrap();
        assert_eq!(previous.results, vec![crossing]);
        assert_eq!(older_search_chunk(older.start, history), 0..2024);
        assert_eq!(previous.by_line.get(&1999).unwrap().len(), 1);
        assert_eq!(previous.by_line.get(&2000).unwrap().len(), 1);
        let mut installed = newest.by_line;
        merge_search_row_matches(&mut installed, previous.by_line);
        let shared = installed.get(&2000).unwrap();
        assert_eq!(shared.len(), 2);
        assert_eq!(
            (shared[0].range.clone(), shared[0].result_index),
            (8..12, 0)
        );
        assert_eq!((shared[1].range.clone(), shared[1].result_index), (0..4, 1));
    }

    #[test]
    fn copy_search_retry_exhaustion_retires_exact_run() {
        let run = search_identity(1, 10, 80);
        let mut searching = Some(pending(run, 0..100));
        let mut failed = None;
        for attempt in 0..MAX_SEARCH_RETRIES {
            searching.as_mut().unwrap().retry_attempt = attempt;
            assert!(!finish_exhausted_search(&mut searching, &mut failed, run));
            assert!(searching.is_some());
            assert!(failed.is_none());
        }
        searching.as_mut().unwrap().retry_attempt = MAX_SEARCH_RETRIES;
        let superseded = search_identity(0, 10, 80);
        assert!(!finish_exhausted_search(
            &mut searching,
            &mut failed,
            superseded
        ));
        assert!(finish_exhausted_search(&mut searching, &mut failed, run));
        assert!(searching.is_none());
        assert_eq!(failed, Some(run));
        assert_eq!(
            classify_search_completion(None, run, &(0..100), run, false),
            SearchCompletionStatus::Superseded
        );
        assert!(!admit_search_restart(&mut failed, true));
        assert_eq!(failed, Some(run));
        assert!(admit_search_restart(&mut failed, false));
        assert!(failed.is_none());
    }

    #[test]
    fn copy_overlay_dirty_rows_are_reported_without_delegate_damage() {
        let overlay_dirty = make_dirty_ranges([12..14]);

        let merged = merge_dirty_results(10..20, RangeSet::default(), &overlay_dirty);

        assert_eq!(collect_ranges(&merged), vec![12..14]);
    }

    #[test]
    fn copy_overlay_dirty_rows_are_clipped_to_requested_range() {
        let delegate_dirty = make_dirty_ranges([5..8, 12..14]);
        let overlay_dirty = make_dirty_ranges([18..22]);

        let merged = merge_dirty_results(10..20, delegate_dirty, &overlay_dirty);

        assert_eq!(collect_ranges(&merged), vec![12..14, 18..20]);
    }

    #[test]
    fn copy_search_row_tracks_viewport_bottom_edge() {
        let dims = dimensions(40, 5);

        assert_eq!(compute_search_row_from_viewport(None, dims), 44);
        assert_eq!(compute_search_row_from_viewport(Some(12), dims), 16);
    }

    #[test]
    fn copy_search_row_fails_closed_when_bottom_would_overflow() {
        let dims = dimensions(StableRowIndex::MAX, 2);

        let row = compute_search_row_from_viewport(Some(StableRowIndex::MAX), dims);
        assert_eq!(row, StableRowIndex::MAX.saturating_sub(1));
        let mut dirty = RangeSet::default();
        dirty.add(row);
        assert_eq!(collect_ranges(&dirty), vec![row..StableRowIndex::MAX]);
    }

    #[test]
    fn copy_search_row_fails_closed_for_oversized_or_empty_viewport() {
        assert_eq!(
            compute_search_row_from_viewport(Some(17), dimensions(0, usize::MAX)),
            17
        );
        assert_eq!(
            compute_search_row_from_viewport(Some(17), dimensions(0, 0)),
            17
        );
    }

    #[test]
    fn copy_page_boundaries_fail_closed_at_stable_row_limits() {
        assert_eq!(checked_page_up_boundary(StableRowIndex::MIN, 1), None);
        assert_eq!(checked_page_down_boundary(StableRowIndex::MAX, 1), None);
    }

    #[test]
    fn copy_page_boundaries_reject_oversized_viewports() {
        assert_eq!(checked_page_up_boundary(17, usize::MAX), None);
        assert_eq!(checked_page_down_boundary(17, usize::MAX), None);
    }

    #[test]
    fn copy_extent_helpers_cover_empty_and_limit_boundaries() {
        assert_eq!(checked_stable_row_end(7, 0), Some(7));
        assert_eq!(checked_last_stable_row(7, 0), None);
        assert_eq!(checked_stable_row_end(StableRowIndex::MAX, 1), None);
        assert_eq!(checked_last_stable_row(StableRowIndex::MAX, 2), None);
        assert_eq!(one_line_range(StableRowIndex::MAX), None);
    }

    #[test]
    fn fractional_page_delta_rejects_oversized_or_non_finite_inputs() {
        assert_eq!(checked_fractional_row_delta(5, 0.5), Some(2));
        assert_eq!(checked_fractional_row_delta(usize::MAX, 1.0), None);
        assert_eq!(checked_fractional_row_delta(5, f64::INFINITY), None);
    }

    fn search_identity(id: usize, seqno: SequenceNo, cols: usize) -> SearchRunIdentity {
        let mut dims = dimensions(0, 5);
        dims.cols = cols;
        search_run_identity(id, seqno, dims)
    }

    fn pending(run: SearchRunIdentity, range: Range<StableRowIndex>) -> Searching {
        Searching {
            remain: 0,
            run,
            range,
            retry_attempt: 0,
        }
    }

    #[test]
    fn copy_search_completion_rejects_same_pattern_from_superseded_run() {
        let old = search_identity(1, 10, 80);
        let current = search_identity(2, 10, 80);
        let pending = pending(current, 0..10);

        assert_eq!(
            classify_search_completion(Some(&pending), old, &(0..10), current, false),
            SearchCompletionStatus::Superseded
        );
    }

    #[test]
    fn copy_search_completion_accepts_only_exact_run_range_and_source() {
        let run = search_identity(2, 10, 80);
        let pending = pending(run, 0..10);

        assert_eq!(
            classify_search_completion(Some(&pending), run, &(0..10), run, false),
            SearchCompletionStatus::Accepted
        );
    }

    #[test]
    fn copy_search_completion_rejects_out_of_order_chunk_range() {
        let run = search_identity(2, 10, 80);
        let pending = pending(run, 0..10);

        assert_eq!(
            classify_search_completion(Some(&pending), run, &(10..20), run, false),
            SearchCompletionStatus::Superseded
        );
    }

    #[test]
    fn copy_search_completion_ignores_unrelated_source_change_but_rejects_dirty_range_or_resize() {
        let run = search_identity(2, 10, 80);
        let pending = pending(run, 0..10);
        let changed_source = search_identity(99, 11, 80);
        let resized = search_identity(99, 10, 120);

        assert_eq!(
            classify_search_completion(Some(&pending), run, &(0..10), changed_source, false),
            SearchCompletionStatus::Accepted
        );
        assert_eq!(
            classify_search_completion(Some(&pending), run, &(0..10), changed_source, true),
            SearchCompletionStatus::SourceChanged
        );
        assert_eq!(
            classify_search_completion(Some(&pending), run, &(0..10), resized, false),
            SearchCompletionStatus::SourceChanged
        );
    }

    #[test]
    fn copy_search_run_id_exhaustion_never_wraps_or_reuses_an_id() {
        let mut next = usize::MAX;
        assert_eq!(allocate_search_run_id(&mut next), None);
        assert_eq!(next, usize::MAX);
    }

    #[test]
    fn previous_word_navigation_respects_negative_scrollback_top() {
        assert_eq!(previous_row_within_scrollback(-5, -10), Some(-6));
        assert_eq!(previous_row_within_scrollback(-10, -10), None);
        assert_eq!(
            previous_row_within_scrollback(StableRowIndex::MIN, StableRowIndex::MIN),
            None
        );
    }

    #[test]
    fn copy_overlay_instance_tokens_do_not_alias_across_reopen() {
        let first = Arc::new(());
        let reopened = Arc::new(());
        assert!(!Arc::ptr_eq(&first, &reopened));
        assert!(Arc::ptr_eq(&first, &Arc::clone(&first)));
    }

    #[test]
    fn copy_damage_transfer_splits_claimed_range_and_preserves_other_consumers() {
        let mut overlay = make_dirty_ranges([1..4, 8..12]);
        let claimed = take_dirty_results(2..10, RangeSet::default(), &mut overlay);
        assert_eq!(collect_ranges(&claimed), vec![2..4, 8..10]);
        assert_eq!(collect_ranges(&overlay), vec![1..2, 10..12]);
    }

    #[test]
    fn copy_damage_pruning_handles_negative_scrollback_and_drops_retired_rows() {
        let mut dirty = make_dirty_ranges([-20..-10, -5..3, 8..12]);
        prune_dirty_results(&mut dirty, Some(-10..10));
        assert_eq!(collect_ranges(&dirty), vec![-5..3, 8..10]);
    }

    #[test]
    fn copy_search_geometry_is_strictly_validated_and_cardinality_is_checked() {
        assert_eq!(validate_search_geometry(7, -20, 90, 20, &(-5..6), 80), None);
        assert_eq!(validate_search_geometry(9, 2, 9, 2, &(0..4), 80), None);
        assert_eq!(validate_search_geometry(0, 9, 1, 2, &(0..10), 80), None);
        assert_eq!(
            validate_search_geometry(7, -5, 80, 5, &(-5..6), 80),
            Some(ValidatedSearchGeometry {
                start_x: 7,
                start_y: -5,
                end_x: 80,
                end_y: 5,
                row_span: 11,
            })
        );
    }

    #[test]
    fn copy_multiline_exclusive_zero_end_selects_previous_row() {
        let result = SearchResult {
            start_y: 2,
            start_x: 7,
            end_y: 3,
            end_x: 0,
            match_id: 1,
        };
        assert_eq!(inclusive_search_result_end(&result, 80), Some((79, 2)));
    }

    #[test]
    fn copy_search_preparation_applies_global_cap_and_stable_indexes() {
        let results = vec![
            SearchResult {
                start_y: 2,
                start_x: 1,
                end_y: 2,
                end_x: 3,
                match_id: 1,
            },
            SearchResult {
                start_y: 3,
                start_x: 4,
                end_y: 3,
                end_x: 6,
                match_id: 2,
            },
        ];
        let cancel = AtomicBool::new(false);
        let prepared = prepare_copy_search_chunk(results, &(0..10), 80, 7, 1, 1, &cancel)
            .expect("uncancelled preparation completes");

        assert_eq!(prepared.results.len(), 1);
        assert_eq!(prepared.expanded_rows, 1);
        let only_match = prepared
            .by_line
            .values()
            .next()
            .and_then(|matches| matches.first())
            .expect("one prepared row match");
        assert_eq!(only_match.result_index, 7);
    }

    #[test]
    fn copy_search_preparation_honors_latest_wins_cancellation() {
        let cancel = AtomicBool::new(true);
        assert!(prepare_copy_search_chunk(Vec::new(), &(0..10), 80, 0, 1, 1, &cancel).is_none());
    }

    #[test]
    fn copy_search_preparation_gate_rejects_overlap_without_queueing() {
        let gate = Mutex::new(());
        let _running = gate.lock();
        assert!(gate.try_lock().is_none());
    }

    #[test]
    fn copy_search_retry_delay_is_bounded() {
        assert_eq!(search_retry_delay(0), Duration::from_millis(50));
        assert_eq!(search_retry_delay(u8::MAX), Duration::from_millis(1000));
    }
}

#[derive(Debug)]
pub struct CopyModeParams {
    pub pattern: Pattern,
    pub editing_search: bool,
}

impl CopyOverlay {
    pub(crate) fn selection_delegate(&self) -> &Arc<dyn Pane> {
        &self.delegate
    }

    pub fn with_pane(
        term_window: &TermWindow,
        pane: &Arc<dyn Pane>,
        params: CopyModeParams,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        let mut cursor = pane.get_cursor_position();
        cursor.shape = termwiz::surface::CursorShape::SteadyBlock;
        cursor.visibility = CursorVisibility::Visible;

        let mux = mux::Mux::try_get()
            .ok_or_else(|| anyhow::anyhow!("cannot start copy overlay without an active mux"))?;
        let (_domain, _window, tab_id) = mux
            .resolve_pane_id(pane.pane_id())
            .ok_or_else(|| anyhow::anyhow!("no tab contains the current pane"))?;

        let window = term_window
            .window
            .clone()
            .ok_or_else(|| anyhow::anyhow!("failed to clone window handle"))?;
        let dims = pane.get_dimensions();
        let pattern = if params.pattern.is_empty() {
            SAVED_PATTERN
                .lock()
                .get(&tab_id)
                .map(|p| p.clone())
                .unwrap_or(params.pattern)
        } else {
            params.pattern
        };
        let search_line = LineEditBuffer::new(&pattern, pattern.len());

        let mut render = CopyRenderable {
            instance_token: Arc::new(()),
            cursor,
            window,
            delegate: Arc::clone(pane),
            start: None,
            viewport: term_window.get_viewport(pane.pane_id()),
            results: vec![],
            result_source_ends: vec![],
            result_source_ranges: vec![],
            expanded_result_rows: 0,
            by_line: HashMap::new(),
            dirty_results: RangeSet::default(),
            width: dims.cols,
            height: dims.viewport_rows,
            last_result_seqno: SEQ_ZERO,
            last_bar_pos: None,
            tab_id,
            pattern_type: PatternType::from(&pattern),
            search_line,
            editing_search: params.editing_search,
            result_pos: None,
            selection_mode: SelectionMode::Cell,
            typing_cookie: 0,
            searching: None,
            search_failed: None,
            next_search_run_id: 0,
            search_abort: None,
            search_preparation_cancel: None,
            search_preparation_gate: Arc::new(Mutex::new(())),
            debounce_abort: None,
            debounce_token: None,
            retry_abort: None,
            retry_token: None,
            desired_result: None,
            desired_result_ordinal: None,
            pending_jump: None,
            last_jump: None,
            content_navigation: None,
            content_action: Arc::new(()),
            content_viewport_publication: Arc::new(ContentViewportPublication::default()),
            coordinate_anchor: None,
            coordinate_driver: None,
            coordinate_action: Arc::new(()),
            deferred_navigation: VecDeque::new(),
            coordinates_invalid: false,
            coordinate_driver_running: Arc::new(AtomicBool::new(false)),
            coordinate_remap_deadline: None,
        };

        let search_row = render.compute_search_row();
        render.dirty_results.add(search_row);
        render.update_search();
        render.refresh_coordinate_anchor();

        let shared_render = Arc::new(Mutex::new(render));
        let writer = SearchOverlayPatternWriter {
            render: Arc::clone(&shared_render),
        };

        Ok(Arc::new(CopyOverlay {
            delegate: Arc::clone(pane),
            render: shared_render,
            writer: Mutex::new(writer),
        }))
    }

    pub fn get_params(&self) -> CopyModeParams {
        let render = self.render.lock();
        CopyModeParams {
            pattern: render.get_pattern(),
            editing_search: render.editing_search,
        }
    }

    pub fn apply_params(&self, params: CopyModeParams) {
        let mut render = self.render.lock();
        render.editing_search = params.editing_search;
        if render.get_pattern() != params.pattern {
            render.pattern_type = PatternType::from(&params.pattern);
            render
                .search_line
                .set_line_and_cursor(&params.pattern, params.pattern.len());
            render.schedule_update_search();
        }
        render.mark_search_ui_dirty();
    }

    pub fn viewport_changed(&self, viewport: Option<StableRowIndex>) {
        let mut render = self.render.lock();
        if render.viewport != viewport {
            if let Some(last) = render.last_bar_pos.take() {
                render.dirty_results.add(last);
            }
            render.viewport = viewport;
            let publication = Arc::clone(&render.content_viewport_publication);
            if publication.update_dirty(viewport, &mut render.dirty_results) {
                // Only the synchronous, source-validated set_viewport call may
                // reuse dimensions without relocking the terminal.
                render.window.invalidate();
            } else {
                render.mark_search_ui_dirty();
            }
            if render.coordinates_current() {
                render.refresh_coordinate_anchor();
            }
        }
    }
}

impl Drop for CopyRenderable {
    fn drop(&mut self) {
        if let Some(cancel) = self.search_preparation_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        if let Some(abort) = self.search_abort.take() {
            abort.abort();
        }
        if let Some(abort) = self.debounce_abort.take() {
            abort.abort();
        }
        if let Some(abort) = self.retry_abort.take() {
            abort.abort();
        }
    }
}

impl CopyRenderable {
    fn coordinates_current(&self) -> bool {
        let Some((authority, _, dimensions)) =
            crate::selection::SelectionAuthority::capture_source(&*self.delegate)
        else {
            return false;
        };
        let Some(end) =
            checked_stable_row_end(dimensions.scrollback_top, dimensions.scrollback_rows)
        else {
            return false;
        };
        !self.coordinates_invalid
            && [
                Some(self.cursor.y),
                self.start.map(|start| start.y),
                self.viewport,
            ]
            .into_iter()
            .flatten()
            .all(|row| row >= dimensions.scrollback_top && row < end)
            && self
                .coordinate_anchor
                .as_ref()
                .is_some_and(|anchor| anchor.allows_navigation(Some(authority)))
    }

    fn refresh_coordinate_anchor(&mut self) {
        use wezterm_term::screen::SelectionAnchorCoordinate as Point;
        let source = crate::selection::SelectionAuthority::capture_source(&*self.delegate)
            .or_else(|| self.coordinate_anchor.as_ref().map(|anchor| anchor.source));
        self.coordinate_driver.take();
        self.coordinate_driver_running = Arc::new(AtomicBool::new(false));
        self.coordinate_remap_deadline = None;
        self.coordinate_action = Arc::new(());
        self.coordinate_anchor.take();
        let Some(source) = source else {
            self.coordinates_invalid = true;
            log::warn!("copy navigation has no current coordinate authority");
            return;
        };
        let points = [
            Some(Point {
                column: Some(self.cursor.x),
                row: self.cursor.y,
            }),
            self.start.map(|start| Point {
                column: match start.x {
                    SelectionX::Cell(x) => Some(x),
                    SelectionX::BeforeZero => None,
                },
                row: start.y,
            }),
            self.viewport.map(|row| Point {
                column: Some(0),
                row,
            }),
        ];
        let mut anchor = CopyCoordinateAnchor {
            source,
            original: source,
            points,
            state: std::array::from_fn(|_| None),
            captured: [false; 3],
            deadline: std::time::Instant::now() + Duration::from_secs(5),
        };
        // First poll orders the capture before a subsequent resize request.
        let initial = anchor.poll(&*self.delegate);
        self.coordinate_anchor = Some(anchor);
        self.coordinates_invalid = false;
        if !matches!(initial, Ok(Some(_))) {
            self.schedule_coordinate_driver();
        }
    }

    fn schedule_coordinate_driver(&mut self) {
        if self.coordinate_driver_running.load(Ordering::Acquire) || self.coordinates_invalid {
            return;
        }
        let deadline = *self
            .coordinate_remap_deadline
            .get_or_insert_with(|| std::time::Instant::now() + Duration::from_secs(5));
        if std::time::Instant::now() >= deadline {
            self.coordinates_invalid = true;
            self.start = None;
            self.deferred_navigation.clear();
            self.clear_selection();
            log::warn!("copy coordinate remap deadline expired; navigation refused");
            return;
        }
        let Some(anchor) = self.coordinate_anchor.as_mut() else {
            return;
        };
        anchor.deadline = deadline;
        let reservation = match super::reserve_overlay_main_thread(
            promise::spawn::MainThreadServiceClass::Interactive,
            4096,
            "copy coordinate remap",
        ) {
            Ok(reservation) => reservation,
            Err(error) => {
                log::warn!("{error:#}; copy coordinate remap not admitted");
                return;
            }
        };
        let window = self.window.clone();
        let pane_id = self.delegate.pane_id();
        let instance = Arc::clone(&self.instance_token);
        let action = Arc::clone(&self.coordinate_action);
        self.coordinate_driver_running = Arc::new(AtomicBool::new(true));
        let running = CopyCoordinateDriverGuard(Arc::clone(&self.coordinate_driver_running));
        self.coordinate_driver = Some(reservation.spawn_local(async move {
            let _running = running;
            loop {
                let (send, recv) = oneshot::channel();
                let instance = Arc::clone(&instance);
                let action = Arc::clone(&action);
                window.notify(TermWindowNotif::Apply(Box::new(move |tw| {
                    let overlay = tw.pane_state(pane_id).and_then(|state| {
                        state
                            .overlay
                            .as_ref()
                            .map(|overlay| Arc::clone(&overlay.pane))
                    });
                    let Some(overlay) = overlay else {
                        return;
                    };
                    let Some(copy) = overlay.downcast_ref::<CopyOverlay>() else {
                        return;
                    };
                    let mut publication = None;
                    let (retry, queued) = {
                        let mut r = copy.render.lock();
                        if !Arc::ptr_eq(&r.instance_token, &instance)
                            || !Arc::ptr_eq(&r.coordinate_action, &action)
                        {
                            return;
                        }
                        let pane = Arc::clone(&r.delegate);
                        let result = r
                            .coordinate_anchor
                            .as_mut()
                            .map(|anchor| anchor.poll(&*pane));
                        match result {
                            Some(Ok(Some((points, source)))) => {
                                let remapped = r
                                    .coordinate_anchor
                                    .as_ref()
                                    .is_some_and(|anchor| anchor.source.0 != source.0);
                                let Some(cursor) = points[0].and_then(|point| {
                                    point.column.map(|column| (column, point.row))
                                }) else {
                                    let _ = send.send(false);
                                    return;
                                };
                                r.cursor.x = cursor.0;
                                r.cursor.y = cursor.1;
                                r.start = points[1].map(|point| SelectionCoordinate {
                                    x: point
                                        .column
                                        .map_or(SelectionX::BeforeZero, SelectionX::Cell),
                                    y: point.row,
                                });
                                r.viewport = points[2].map(|point| point.row);
                                r.coordinate_anchor.as_mut().unwrap().source = source;
                                r.coordinates_invalid = false;
                                r.coordinate_driver_running.store(false, Ordering::Release);
                                r.coordinate_remap_deadline = None;
                                if remapped {
                                    publication = Some((
                                        source,
                                        r.viewport,
                                        Arc::clone(&r.content_viewport_publication),
                                        pane,
                                    ));
                                }
                                (false, r.deferred_navigation.drain(..).collect::<Vec<_>>())
                            }
                            Some(Ok(None) | Err(mux::pane::PaneSelectionAnchorError::Busy)) => {
                                (true, Vec::new())
                            }
                            failed => {
                                r.coordinate_driver_running.store(false, Ordering::Release);
                                let current = crate::selection::SelectionAuthority::capture(&*pane);
                                let invalid = match (failed, r.coordinate_anchor.as_ref()) {
                                    (Some(Err(error)), Some(anchor)) => {
                                        anchor.failure_invalidates(error, current)
                                    }
                                    _ => true,
                                };
                                if invalid {
                                    r.coordinates_invalid = true;
                                    r.start = None;
                                    r.clear_selection();
                                    r.deferred_navigation.clear();
                                    log::warn!(
                                        "copy coordinates could not be remapped; navigation refused"
                                    );
                                }
                                (false, Vec::new())
                            }
                        }
                    };
                    if let Some((source, viewport, guard, pane)) = publication {
                        if crate::selection::SelectionAuthority::capture_source(&*pane)
                            == Some(source)
                        {
                            // The GUI selection retains its own original token.
                            // Do not replace that intent with a freshly captured
                            // range derived from this independent viewport token.
                            tw.selection_authority_is_current(&pane);
                            guard.with_dimensions(source.2, || {
                                tw.set_viewport(pane_id, viewport, source.2)
                            });
                        } else {
                            copy.render.lock().schedule_coordinate_driver();
                        }
                    }
                    for assignment in queued {
                        copy.perform_assignment(&assignment);
                    }
                    let _ = send.send(retry);
                })));
                if !coordinate_reply_before_deadline(recv, deadline).await {
                    break;
                }
                sleep(Duration::from_millis(20)).await;
            }
        }));
    }

    fn compute_search_row(&self) -> StableRowIndex {
        compute_search_row_from_viewport(self.viewport, self.delegate.get_dimensions())
    }

    fn update_dimensions(&mut self) -> bool {
        let dims = self.delegate.get_dimensions();
        if dims.cols == self.width && dims.viewport_rows == self.height {
            return false;
        }

        self.width = dims.cols;
        self.height = dims.viewport_rows;
        true
    }

    fn mark_search_ui_dirty(&mut self) {
        if let Some(last) = self.last_bar_pos {
            self.dirty_results.add(last);
        }
        self.dirty_results.add(self.compute_search_row());
        let retained = retained_row_range(self.delegate.get_dimensions());
        prune_dirty_results(&mut self.dirty_results, retained);
        self.window.invalidate();
    }

    fn cancel_search_task(&mut self) {
        if let Some(cancel) = self.search_preparation_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        if let Some(abort) = self.search_abort.take() {
            abort.abort();
        }
    }

    fn cancel_debounce(&mut self) {
        if let Some(abort) = self.debounce_abort.take() {
            abort.abort();
        }
        self.debounce_token.take();
    }

    fn cancel_retry(&mut self) {
        if let Some(abort) = self.retry_abort.take() {
            abort.abort();
        }
        self.retry_token.take();
    }

    fn prepare_for_render(&mut self, lines: Range<StableRowIndex>) {
        let resized = self.update_dimensions();
        let source_changed = if resized || self.searching.is_some() || self.get_pattern().is_empty()
        {
            false
        } else {
            let (source_end, dirty) = self
                .delegate
                .get_changed_since_with_source_fence(lines, self.last_result_seqno);
            source_end < self.last_result_seqno || dirty.iter().next().is_some()
        };
        if copy_resize_restarts_search(resized, self.get_pattern().is_empty()) || source_changed {
            self.restart_search(true, 0);
        }
        if resized || !self.coordinates_current() {
            self.schedule_coordinate_driver();
        }
    }

    fn install_prepared_search_chunk(
        &mut self,
        mut prepared: PreparedCopyChunk,
        source_end: SequenceNo,
        source_range: Range<StableRowIndex>,
    ) {
        let result_count = prepared.results.len();
        // Start-row ownership is disjoint; wrapped matches can still share
        // covered rows with another chunk. Preserve both highlight lists.
        merge_search_row_matches(&mut self.by_line, prepared.by_line);
        self.dirty_results.add_set(&prepared.dirty_rows);
        self.expanded_result_rows = self
            .expanded_result_rows
            .saturating_add(prepared.expanded_rows)
            .min(MAX_TOTAL_EXPANDED_SEARCH_ROWS);
        self.result_source_ends
            .extend(std::iter::repeat(source_end).take(result_count));
        self.result_source_ranges
            .extend(std::iter::repeat_with(|| source_range.clone()).take(result_count));
        self.results.append(&mut prepared.results);
    }

    fn schedule_update_search(&mut self) {
        let reservation = match super::reserve_overlay_main_thread(
            promise::spawn::MainThreadServiceClass::Render,
            4 * 1024,
            "copy-search debounce",
        ) {
            Ok(reservation) => reservation,
            Err(err) => {
                log::error!("{err:#}; retained the current search state for the next input");
                return;
            }
        };
        self.mark_search_ui_dirty();
        // The debounce delays only the replacement query; obsolete search and
        // CPU-preparation work should stop as soon as the pattern changes.
        self.cancel_search_task();
        self.searching.take();
        self.cancel_debounce();
        self.cancel_retry();
        let Some(next_cookie) = self.typing_cookie.checked_add(1) else {
            // Exhaustion is sticky. The exact instance token still prevents a
            // cross-overlay ABA; run synchronously rather than reusing a cookie.
            self.restart_search(false, 0);
            return;
        };
        self.typing_cookie = next_cookie;
        let cookie = self.typing_cookie;

        let window = self.window.clone();
        let pane_id = self.delegate.pane_id();
        let instance_token = Arc::clone(&self.instance_token);
        let debounce_token = Arc::new(());
        self.debounce_token = Some(Arc::clone(&debounce_token));
        let (abort, registration) = AbortHandle::new_pair();
        self.debounce_abort = Some(abort);

        reservation
            .spawn_local(async move {
                if Abortable::new(sleep(Duration::from_millis(350)), registration)
                    .await
                    .is_err()
                {
                    return anyhow::Result::<()>::Ok(());
                }
                window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                    let Some(state) = term_window.pane_state(pane_id) else {
                        return;
                    };
                    if let Some(overlay) = state.overlay.as_ref() {
                        if let Some(copy_overlay) = overlay.pane.downcast_ref::<CopyOverlay>() {
                            let mut r = copy_overlay.render.lock();
                            if Arc::ptr_eq(&r.instance_token, &instance_token)
                                && cookie == r.typing_cookie
                                && r.debounce_token
                                    .as_ref()
                                    .is_some_and(|current| Arc::ptr_eq(current, &debounce_token))
                            {
                                r.debounce_abort.take();
                                r.debounce_token.take();
                                r.restart_search(false, 0);
                            }
                        }
                    }
                })));
                anyhow::Result::<()>::Ok(())
            })
            .detach();
    }

    fn update_search(&mut self) {
        self.restart_search(false, 0);
    }

    fn restart_search(&mut self, preserve_result: bool, retry_attempt: u8) {
        if !admit_search_restart(&mut self.search_failed, preserve_result) {
            return;
        }
        self.cancel_search_task();
        self.cancel_debounce();
        self.cancel_retry();
        self.searching.take();

        if preserve_result && self.desired_result.is_none() {
            self.desired_result_ordinal = self.result_pos;
            self.desired_result = self
                .result_pos
                .and_then(|position| self.results.get(position))
                .map(search_result_anchor);
        } else if !preserve_result {
            self.desired_result.take();
            self.desired_result_ordinal.take();
        }
        for idx in self.by_line.keys() {
            self.dirty_results.add(*idx);
        }
        if let Some(idx) = self.last_bar_pos.as_ref() {
            self.dirty_results.add(*idx);
        }

        self.results.clear();
        self.result_source_ends.clear();
        self.result_source_ranges.clear();
        self.expanded_result_rows = 0;
        self.by_line.clear();
        self.result_pos.take();

        SAVED_PATTERN.lock().insert(self.tab_id, self.get_pattern());

        let dims = self.delegate.get_dimensions();
        self.width = dims.cols;
        self.height = dims.viewport_rows;
        let bar_pos = self.compute_search_row();
        self.dirty_results.add(bar_pos);
        let retained = retained_row_range(dims);
        prune_dirty_results(&mut self.dirty_results, retained);
        let Some(run_id) = allocate_search_run_id(&mut self.next_search_run_id) else {
            self.last_result_seqno = self.delegate.get_current_seqno();
            self.searching.take();
            self.clear_selection();
            self.window.invalidate();
            return;
        };

        let pattern = self.get_pattern();
        if !pattern.is_empty() {
            let pane: Arc<dyn Pane> = self.delegate.clone();
            let window = self.window.clone();
            let source_seqno = pane.get_current_seqno();
            let run = search_run_identity(run_id, source_seqno, dims);
            self.last_result_seqno = source_seqno;

            let Some(end) = checked_stable_row_end(dims.scrollback_top, dims.scrollback_rows)
            else {
                self.searching.take();
                self.clear_selection();
                self.window.invalidate();
                return;
            };
            let range = end
                .saturating_sub(SEARCH_CHUNK_SIZE)
                .max(dims.scrollback_top)..end;

            self.searching.replace(Searching {
                remain: range.start.saturating_sub(dims.scrollback_top),
                run,
                range: range.clone(),
                retry_attempt,
            });
            self.spawn_search_chunk(pane, window, run, pattern, range, end);
        } else {
            self.last_result_seqno = self.delegate.get_current_seqno();
            self.searching.take();
            self.clear_selection();
        }
        self.window.invalidate();
    }

    fn spawn_search_chunk(
        &mut self,
        pane: Arc<dyn Pane>,
        window: ::window::Window,
        run: SearchRunIdentity,
        pattern: Pattern,
        range: Range<StableRowIndex>,
        owned_end: StableRowIndex,
    ) {
        let reservation = match super::reserve_overlay_main_thread(
            promise::spawn::MainThreadServiceClass::Interactive,
            32 * 1024,
            "copy-search chunk",
        ) {
            Ok(reservation) => reservation,
            Err(err) => {
                self.searching.take();
                self.mark_search_ui_dirty();
                log::error!("{err:#}; ended the exact copy-search run before task construction");
                return;
            }
        };
        self.cancel_search_task();
        let instance_token = Arc::clone(&self.instance_token);
        let result_base = self.results.len();
        let result_capacity = MAX_TOTAL_SEARCH_RESULTS.saturating_sub(result_base);
        let expanded_row_capacity =
            MAX_TOTAL_EXPANDED_SEARCH_ROWS.saturating_sub(self.expanded_result_rows);
        let preparation_cancel = Arc::new(AtomicBool::new(false));
        self.search_preparation_cancel = Some(Arc::clone(&preparation_cancel));
        let preparation_gate = Arc::clone(&self.search_preparation_gate);
        let (abort, registration) = AbortHandle::new_pair();
        self.search_abort = Some(abort);
        reservation
            .spawn_local(async move {
                let limit = Some(SEARCH_RESULT_REQUEST_LIMIT_PER_CHUNK);
                log::trace!("Searching for {pattern:?} in {range:?}");
                let preparation_range = range.clone();
                let completion = Abortable::new(
                    async {
                        let results = pane
                            .search(pattern.clone(), range.clone(), limit)
                            .await
                            .map_err(|err| format!("{err:#}"))?;
                        prepare_copy_search_chunk_off_thread(
                            results,
                            preparation_range,
                            owned_end,
                            run.cols,
                            result_base,
                            result_capacity,
                            expanded_row_capacity,
                            preparation_cancel,
                            preparation_gate,
                        )
                        .await
                    },
                    registration,
                )
                .await;
                let outcome = match completion {
                    Err(_) => return anyhow::Result::<()>::Ok(()),
                    Ok(outcome) => outcome,
                };

                let pane_id = pane.pane_id();
                let mut outcome = Some(outcome);
                window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                    let Some(state) = term_window.pane_state(pane_id) else { return; };
                    if let Some(overlay) = state.overlay.as_ref() {
                        if let Some(copy_overlay) = overlay.pane.downcast_ref::<CopyOverlay>() {
                            let mut renderer = copy_overlay.render.lock();
                            if !Arc::ptr_eq(&renderer.instance_token, &instance_token) {
                                return;
                            }
                            let Some(outcome) = outcome.take() else {
                                log::warn!(
                                "copy overlay search completion already consumed for pane {pane_id}"
                            );
                                return;
                            };
                            match outcome {
                                Ok(prepared) => {
                                    renderer.processed_search_chunk(run, pattern, prepared, range)
                                }
                                Err(error) => {
                                    renderer.processed_search_error(run, range, error);
                                }
                            }
                        }
                    }
                })));
                anyhow::Result::<()>::Ok(())
            })
            .detach();
    }

    fn schedule_search_retry(&mut self, run: SearchRunIdentity, retry_attempt: u8) {
        if finish_exhausted_search(&mut self.searching, &mut self.search_failed, run) {
            self.cancel_search_task();
            self.cancel_retry();
            for row in self.by_line.keys() {
                self.dirty_results.add(*row);
            }
            self.results.clear();
            self.result_source_ends.clear();
            self.result_source_ranges.clear();
            self.by_line.clear();
            self.expanded_result_rows = 0;
            self.result_pos.take();
            self.desired_result.take();
            self.desired_result_ordinal.take();
            self.clear_selection();
            self.mark_search_ui_dirty();
            return;
        }
        let reservation = match super::reserve_overlay_main_thread(
            promise::spawn::MainThreadServiceClass::Render,
            4 * 1024,
            "copy-search retry",
        ) {
            Ok(reservation) => reservation,
            Err(err) => {
                self.cancel_search_task();
                self.cancel_retry();
                self.searching.take();
                self.mark_search_ui_dirty();
                log::error!("{err:#}; ended the exact copy-search run instead of losing a retry");
                return;
            }
        };
        self.cancel_search_task();
        self.cancel_retry();
        let delay = search_retry_delay(retry_attempt);
        let next_attempt = retry_attempt.saturating_add(1);
        let pane_id = self.delegate.pane_id();
        let window = self.window.clone();
        let instance_token = Arc::clone(&self.instance_token);
        let retry_token = Arc::new(());
        self.retry_token = Some(Arc::clone(&retry_token));
        let (abort, registration) = AbortHandle::new_pair();
        self.retry_abort = Some(abort);
        self.mark_search_ui_dirty();
        reservation
            .spawn_local(async move {
                if Abortable::new(sleep(delay), registration).await.is_err() {
                    return anyhow::Result::<()>::Ok(());
                }
                window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                    let Some(state) = term_window.pane_state(pane_id) else {
                        return;
                    };
                    if let Some(overlay) = state.overlay.as_ref() {
                        if let Some(copy_overlay) = overlay.pane.downcast_ref::<CopyOverlay>() {
                            let mut renderer = copy_overlay.render.lock();
                            if Arc::ptr_eq(&renderer.instance_token, &instance_token)
                                && renderer
                                    .searching
                                    .as_ref()
                                    .is_some_and(|pending| pending.run == run)
                                && renderer
                                    .retry_token
                                    .as_ref()
                                    .is_some_and(|current| Arc::ptr_eq(current, &retry_token))
                            {
                                renderer.retry_abort.take();
                                renderer.retry_token.take();
                                renderer.restart_search(true, next_attempt);
                            }
                        }
                    }
                })));
                anyhow::Result::<()>::Ok(())
            })
            .detach();
    }

    fn processed_search_error(
        &mut self,
        run: SearchRunIdentity,
        range: Range<StableRowIndex>,
        error: String,
    ) {
        let Some(pending) = self.searching.as_ref() else {
            return;
        };
        if pending.run != run || pending.range != range {
            return;
        }
        let retry_attempt = pending.retry_attempt;
        self.search_abort.take();
        self.search_preparation_cancel.take();
        if error.starts_with("copy search preparation ") {
            log::debug!("copy overlay search preparation deferred for {range:?}: {error}");
        } else {
            log::warn!("copy overlay search failed for {range:?}: {error}");
        }
        self.schedule_search_retry(run, retry_attempt);
    }

    fn processed_search_chunk(
        &mut self,
        run: SearchRunIdentity,
        pattern: Pattern,
        prepared: PreparedCopyChunk,
        range: Range<StableRowIndex>,
    ) {
        let dims = self.delegate.get_dimensions();
        // Validate only the chunk that the backend searched. Re-fencing the
        // growing union here would make live output in the newest chunk starve
        // every older chunk and can turn a large search into quadratic work.
        // Per-result chunk receipts below keep actions fail-closed; an atomic
        // whole-scrollback snapshot requires a mux search-snapshot API.
        let (source_end, source_dirty) = self
            .delegate
            .get_changed_since_with_source_fence(range.clone(), run.source_seqno);
        let current_source = search_run_identity(run.id, source_end, dims);
        match classify_search_completion(
            self.searching.as_ref(),
            run,
            &range,
            current_source,
            source_end < run.source_seqno || source_dirty.iter().next().is_some(),
        ) {
            SearchCompletionStatus::Accepted => {}
            SearchCompletionStatus::Superseded => return,
            SearchCompletionStatus::SourceChanged => {
                let retry_attempt = self
                    .searching
                    .as_ref()
                    .map(|pending| pending.retry_attempt)
                    .unwrap_or_default();
                self.schedule_search_retry(run, retry_attempt);
                return;
            }
        }
        self.search_abort.take();
        self.search_preparation_cancel.take();
        if pattern != self.get_pattern() {
            return;
        }
        self.window.invalidate();
        let had_no_results = self.results.is_empty();
        self.install_prepared_search_chunk(prepared, source_end, range.clone());

        if let Some(desired) = self.desired_result {
            if let Some(position) = self
                .results
                .iter()
                .position(|result| search_result_anchor(result) == desired)
            {
                self.desired_result.take();
                self.desired_result_ordinal.take();
                self.activate_match_number_unchecked(position);
            }
        } else if had_no_results && !self.results.is_empty() {
            self.activate_match_number_unchecked(0);
        }

        if self.results.len() >= MAX_TOTAL_SEARCH_RESULTS
            || self.expanded_result_rows >= MAX_TOTAL_EXPANDED_SEARCH_ROWS
        {
            // The retained prefix is deterministic. Do not keep walking a
            // huge scrollback after the overlay's explicit memory/cardinality
            // envelope has been filled.
            self.searching.take();
            self.desired_result.take();
            if self.result_pos.is_none() && !self.results.is_empty() {
                let fallback = self
                    .desired_result_ordinal
                    .take()
                    .unwrap_or_default()
                    .min(self.results.len().saturating_sub(1));
                self.activate_match_number_unchecked(fallback);
            } else {
                self.desired_result_ordinal.take();
            }
            self.mark_search_ui_dirty();
            return;
        }

        if range.start == dims.scrollback_top {
            self.searching.take();
            if self.desired_result.take().is_some() && !self.results.is_empty() {
                let fallback = self
                    .desired_result_ordinal
                    .take()
                    .unwrap_or_default()
                    .min(self.results.len().saturating_sub(1));
                self.activate_match_number_unchecked(fallback);
            } else {
                self.desired_result_ordinal.take();
            }
            if self.results.is_empty() {
                self.set_viewport(None);
                self.clear_selection();
            }
            self.mark_search_ui_dirty();
            return;
        }

        // Search next chunk
        let pane: Arc<dyn Pane> = self.delegate.clone();
        let window = self.window.clone();
        let owned_end = range.start;
        let Some(history_end) = checked_stable_row_end(dims.scrollback_top, dims.scrollback_rows)
        else {
            self.searching.take();
            self.mark_search_ui_dirty();
            return;
        };
        let range = older_search_chunk(owned_end, dims.scrollback_top..history_end);

        let next_run = search_run_identity(run.id, source_end, dims);
        let retry_attempt = self
            .searching
            .as_ref()
            .map(|pending| pending.retry_attempt)
            .unwrap_or_default();
        self.searching.replace(Searching {
            remain: range.start.saturating_sub(dims.scrollback_top),
            run: next_run,
            range: range.clone(),
            retry_attempt,
        });
        self.spawn_search_chunk(pane, window, next_run, pattern, range, owned_end);
        self.mark_search_ui_dirty();
    }

    fn clear_selection(&mut self) {
        let pane = Arc::clone(&self.delegate);
        self.window
            .notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                term_window.clear_selection(&pane);
            })));
    }

    /// Revalidate the exact chunk fence immediately before a user action uses
    /// one of its coordinates. Rendering validates visible rows separately;
    /// this closes the off-screen acceptance-to-navigation gap for a long,
    /// incrementally searched scrollback.
    fn result_is_current(&mut self, n: usize) -> bool {
        let Some(result) = self.results.get(n).cloned() else {
            return false;
        };
        let Some(source_end) = self.result_source_ends.get(n).copied() else {
            return false;
        };
        let Some(source_range) = self.result_source_ranges.get(n).cloned() else {
            return false;
        };
        let dims = self.delegate.get_dimensions();
        let retained = retained_row_range(dims);
        let chunk_is_retained = retained.as_ref().is_some_and(|retained| {
            retained.start <= source_range.start && source_range.end <= retained.end
        });
        if dims.cols != self.width || dims.viewport_rows != self.height || !chunk_is_retained {
            self.desired_result = Some(search_result_anchor(&result));
            self.desired_result_ordinal = Some(n);
            self.restart_search(true, 0);
            return false;
        }
        let (next_source_end, dirty) = self
            .delegate
            .get_changed_since_with_source_fence(source_range, source_end);
        if next_source_end < source_end || dirty.iter().next().is_some() {
            self.desired_result = Some(search_result_anchor(&result));
            self.desired_result_ordinal = Some(n);
            self.restart_search(true, 0);
            return false;
        }
        self.result_source_ends[n] = next_source_end;
        true
    }

    fn activate_match_number(&mut self, n: usize) {
        if self.result_is_current(n) {
            self.activate_match_number_unchecked(n);
        }
    }

    fn activate_match_number_unchecked(&mut self, n: usize) {
        let Some(result) = self.results.get(n).cloned() else {
            return;
        };
        let Some((inclusive_end_x, inclusive_end_y)) =
            inclusive_search_result_end(&result, self.width)
        else {
            return;
        };
        self.result_pos.replace(n);
        self.cursor.y = inclusive_end_y;
        self.cursor.x = inclusive_end_x;

        let start = SelectionCoordinate::x_y(result.start_x, result.start_y);
        let end = SelectionCoordinate::x_y(inclusive_end_x, inclusive_end_y);
        self.start.replace(start);
        self.adjust_selection(start, SelectionRange { start, end });
    }

    fn clamp_cursor_to_scrollback(&mut self) {
        let dims = self.delegate.get_dimensions();
        if dims.cols == 0 {
            self.cursor.x = 0;
        } else if self.cursor.x >= dims.cols {
            self.cursor.x = dims.cols.saturating_sub(1);
        }
        if self.cursor.y < dims.scrollback_top {
            self.cursor.y = dims.scrollback_top;
        }

        let Some(last_row) = checked_last_stable_row(dims.scrollback_top, dims.scrollback_rows)
        else {
            self.cursor.y = dims.scrollback_top;
            return;
        };
        if self.cursor.y > last_row {
            self.cursor.y = last_row;
        }
    }

    fn select_to_cursor_pos(&mut self) {
        self.content_navigation.take();
        self.content_action = Arc::new(());
        self.clamp_cursor_to_scrollback();
        if let Some(sel_start) = self.start {
            let cursor = SelectionCoordinate::x_y(self.cursor.x, self.cursor.y);

            let (start, end) = match self.selection_mode {
                SelectionMode::Line => {
                    let cursor_is_above_start = self.cursor.y < sel_start.y;

                    let start = SelectionCoordinate::x_y(
                        if cursor_is_above_start { usize::MAX } else { 0 },
                        sel_start.y,
                    );
                    let end = SelectionCoordinate::x_y(
                        if cursor_is_above_start { 0 } else { usize::MAX },
                        self.cursor.y,
                    );
                    (start, end)
                }
                SelectionMode::SemanticZone => {
                    let zone_range = SelectionRange::zone_around(cursor, &*self.delegate);
                    let start_zone = SelectionRange::zone_around(sel_start, &*self.delegate);

                    let range = zone_range.extend_with(start_zone);

                    (range.start, range.end)
                }
                _ => {
                    let start = SelectionCoordinate {
                        x: sel_start.x,
                        y: sel_start.y,
                    };
                    let end = cursor;
                    (start, end)
                }
            };

            self.adjust_selection(start, SelectionRange { start, end });
        } else {
            self.adjust_viewport_for_cursor_position();
            self.window.invalidate();
        }
        self.refresh_coordinate_anchor();
    }

    fn adjust_selection(&self, start: SelectionCoordinate, range: SelectionRange) {
        let pane = Arc::clone(&self.delegate);
        let authority = crate::selection::SelectionAuthority::capture(&*pane);
        let mode = self.selection_mode;
        self.window
            .notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                term_window.update_selection(&pane, authority, |selection| {
                    selection.origin = Some(start);
                    selection.range = Some(range);
                    selection.rectangular = mode == SelectionMode::Block;
                });
            })));
        self.adjust_viewport_for_cursor_position();
    }

    fn dimensions(&self) -> Dimensions {
        const VERTICAL_GAP: isize = 5;
        let dims = self.delegate.get_dimensions();
        let vertical_gap = if dims.physical_top <= VERTICAL_GAP {
            1
        } else {
            VERTICAL_GAP
        };
        let top = self.viewport.unwrap_or_else(|| dims.physical_top);
        Dimensions {
            vertical_gap,
            top,
            dims,
        }
    }

    fn adjust_viewport_for_cursor_position(&self) {
        let dims = self.dimensions();

        if dims.top > self.cursor.y {
            // Cursor is off the top of the viewport; adjust
            self.set_viewport(Some(self.cursor.y.saturating_sub(dims.vertical_gap)));
            return;
        }

        let top_gap = self
            .cursor
            .y
            .checked_sub(dims.top)
            .unwrap_or(StableRowIndex::MAX);
        if top_gap < dims.vertical_gap {
            // Increase the gap so we can "look ahead"
            self.set_viewport(Some(self.cursor.y.saturating_sub(dims.vertical_gap)));
            return;
        }

        let Some(viewport_rows) = StableRowIndex::try_from(dims.dims.viewport_rows).ok() else {
            return;
        };
        let bottom_gap = viewport_rows.saturating_sub(top_gap);
        if bottom_gap < dims.vertical_gap {
            let Some(adjustment) = dims.vertical_gap.checked_sub(bottom_gap) else {
                return;
            };
            let Some(next_top) = dims.top.checked_add(adjustment) else {
                return;
            };
            self.set_viewport(Some(next_top));
        }
    }

    fn set_viewport(&self, row: Option<StableRowIndex>) {
        let dims = self.delegate.get_dimensions();
        let pane_id = self.delegate.pane_id();
        self.window
            .notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                term_window.set_viewport(pane_id, row, dims);
            })));
    }

    fn close(&self) {
        if let Some(pending) = &self.content_navigation {
            pending.cancelled.store(true, Ordering::Release);
        }
        let pane_id = self.delegate.pane_id();
        let instance_token = Arc::clone(&self.instance_token);
        self.window
            .notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                close_copy_overlay_if_current(term_window, pane_id, &instance_token);
            })));
    }

    fn move_by_page(&mut self, amount: f64) {
        let dims = self.dimensions();
        let Some(rows) = checked_fractional_row_delta(dims.dims.viewport_rows, amount) else {
            return;
        };
        let Some(next_row) = self.cursor.y.checked_add(rows) else {
            return;
        };
        self.cursor.y = next_row;
        self.select_to_cursor_pos();
    }

    /// Move to next match
    fn next_match(&mut self) {
        if let Some(cur) = self.result_pos.as_ref() {
            let prior = if *cur > 0 {
                cur.saturating_sub(1)
            } else {
                self.results.len().saturating_sub(1)
            };
            self.activate_match_number(prior);
        }
    }

    /// Move to prior match
    fn prior_match(&mut self) {
        if let Some(cur) = self.result_pos.as_ref() {
            let next = if cur
                .checked_add(1)
                .is_none_or(|next| next >= self.results.len())
            {
                0
            } else {
                cur.saturating_add(1)
            };
            self.activate_match_number(next);
        }
    }

    /// Skip this page of matches and move down to the first match from
    /// the next page.
    fn next_match_page(&mut self) {
        let dims = self.delegate.get_dimensions();
        if let Some(cur) = self.result_pos {
            let top = self.viewport.unwrap_or(dims.physical_top);
            let Some(prior) = checked_page_up_boundary(top, dims.viewport_rows) else {
                return;
            };
            if let Some(pos) = self
                .results
                .iter()
                .position(|res| res.start_y > prior && res.start_y < top)
            {
                self.activate_match_number(pos);
            } else {
                self.activate_match_number(cur.saturating_sub(1));
            }
        }
    }

    /// Skip this page of matches and move up to the first match from
    /// the prior page.
    fn prior_match_page(&mut self) {
        let dims = self.delegate.get_dimensions();
        if let Some(cur) = self.result_pos {
            let top = self.viewport.unwrap_or(dims.physical_top);
            let Some(bottom) = checked_page_down_boundary(top, dims.viewport_rows) else {
                return;
            };
            if let Some(pos) = self.results.iter().position(|res| res.start_y >= bottom) {
                self.activate_match_number(pos);
            } else {
                let len = self.results.len().saturating_sub(1);
                self.activate_match_number(cur.min(len));
            }
        }
    }

    fn get_pattern(&self) -> Pattern {
        let pattern = self.search_line.get_line().to_string();
        match self.pattern_type {
            PatternType::CaseSensitiveString => Pattern::CaseSensitiveString(pattern),
            PatternType::CaseInSensitiveString => Pattern::CaseInSensitiveString(pattern),
            PatternType::Regex => Pattern::Regex(pattern),
        }
    }

    fn clear_pattern(&mut self) {
        self.search_line.clear();
        self.update_search();
    }

    fn edit_pattern(&mut self) {
        self.editing_search = true;
        self.mark_search_ui_dirty();
        self.update_key_table();
    }

    fn accept_pattern(&mut self) {
        self.editing_search = false;
        self.mark_search_ui_dirty();
        self.update_key_table();
    }

    fn update_key_table(&mut self) {
        let window = self.window.clone();
        let pane_id = self.delegate.pane_id();

        window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
            let Some(mut state) = term_window.pane_state(pane_id) else {
                return;
            };
            if let Some(overlay) = state.overlay.as_mut() {
                if let Some(copy_overlay) = overlay.pane.downcast_ref::<CopyOverlay>() {
                    let editing_search = copy_overlay.render.lock().editing_search;

                    overlay.key_table_state.activate(KeyTableArgs {
                        name: if editing_search {
                            "search_mode"
                        } else {
                            "copy_mode"
                        },
                        timeout_milliseconds: None,
                        replace_current: true,
                        one_shot: false,
                        until_unknown: false,
                        prevent_fallback: false,
                    });
                }
            }
        })));
    }

    fn cycle_match_type(&mut self) {
        let pattern_type = match &self.pattern_type {
            PatternType::CaseSensitiveString => PatternType::CaseInSensitiveString,
            PatternType::CaseInSensitiveString => PatternType::Regex,
            PatternType::Regex => PatternType::CaseSensitiveString,
        };
        self.pattern_type = pattern_type;
        self.schedule_update_search();
    }

    fn move_to_viewport_middle(&mut self) {
        let dims = self.dimensions();
        let Some(viewport_rows) = StableRowIndex::try_from(dims.dims.viewport_rows).ok() else {
            return;
        };
        let Some(row) = dims.top.checked_add(viewport_rows / 2) else {
            return;
        };
        self.cursor.y = row;
        self.select_to_cursor_pos();
    }

    fn move_to_viewport_top(&mut self) {
        let dims = self.dimensions();
        let Some(row) = dims.top.checked_add(dims.vertical_gap) else {
            return;
        };
        self.cursor.y = row;
        self.select_to_cursor_pos();
    }

    fn move_to_viewport_bottom(&mut self) {
        let dims = self.dimensions();
        let Some(viewport_rows) = StableRowIndex::try_from(dims.dims.viewport_rows).ok() else {
            return;
        };
        let Some(offset) = viewport_rows.checked_sub(dims.vertical_gap) else {
            return;
        };
        let Some(row) = dims.top.checked_add(offset) else {
            return;
        };
        self.cursor.y = row;
        self.select_to_cursor_pos();
    }

    fn move_left_single_cell(&mut self) {
        self.cursor.x = self.cursor.x.saturating_sub(1);
        self.select_to_cursor_pos();
    }

    fn move_right_single_cell(&mut self) {
        self.cursor.x = self.cursor.x.saturating_add(1);
        self.select_to_cursor_pos();
    }

    fn move_up_single_row(&mut self) {
        self.cursor.y = self.cursor.y.saturating_sub(1);
        self.select_to_cursor_pos();
    }

    fn move_down_single_row(&mut self) {
        self.cursor.y = self.cursor.y.saturating_add(1);
        self.select_to_cursor_pos();
    }
    fn move_to_start_of_line(&mut self) {
        self.cursor.x = 0;
        self.select_to_cursor_pos();
    }

    fn move_to_start_of_next_line(&mut self) {
        self.cursor.x = 0;
        self.cursor.y = self.cursor.y.saturating_add(1);
        self.select_to_cursor_pos();
    }

    fn move_to_top(&mut self) {
        self.cursor.y = self.delegate.get_dimensions().scrollback_top;
        self.select_to_cursor_pos();
    }

    fn move_to_bottom(&mut self) {
        // This will get fixed up by clamp_cursor_to_scrollback
        self.cursor.y = isize::MAX;
        self.select_to_cursor_pos();
    }

    fn start_content_navigation(&mut self, end: bool) -> bool {
        // Remote panes and semantic-zone expansion retain their existing path;
        // neither has the bounded native row/metadata contract used here.
        if self
            .delegate
            .downcast_ref::<mux::localpane::LocalPane>()
            .is_none()
            || self.selection_mode == SelectionMode::SemanticZone
        {
            return false;
        }
        let reservation = match super::reserve_overlay_main_thread(
            promise::spawn::MainThreadServiceClass::Interactive,
            4 * 1024,
            "copy content navigation",
        ) {
            Ok(reservation) => reservation,
            Err(error) => {
                self.content_navigation.take();
                log::error!("{error:#}; copy content navigation was not accepted");
                return true;
            }
        };
        self.content_navigation = Some(ContentNavigation::new(
            (self.cursor.x, self.cursor.y),
            self.start,
            self.selection_mode,
            end,
        ));
        self.content_action = Arc::clone(&self.content_navigation.as_ref().unwrap().token);
        let token = Arc::clone(&self.content_action);
        let instance = Arc::clone(&self.instance_token);
        let pane_id = self.delegate.pane_id();
        let window = self.window.clone();
        reservation
            .spawn_local(async move {
                loop {
                    let (send, recv) = oneshot::channel();
                    let token = Arc::clone(&token);
                    let instance = Arc::clone(&instance);
                    window.notify(TermWindowNotif::Apply(Box::new(move |tw| {
                        let render = {
                            let Some(state) = tw.pane_state(pane_id) else {
                                return;
                            };
                            let Some(copy) = state
                                .overlay
                                .as_ref()
                                .and_then(|overlay| overlay.pane.downcast_ref::<CopyOverlay>())
                            else {
                                return;
                            };
                            Arc::clone(&copy.render)
                        };
                        let (retry, effect) = {
                            let mut r = render.lock();
                            if !Arc::ptr_eq(&r.instance_token, &instance)
                                || !Arc::ptr_eq(&r.content_action, &token)
                            {
                                return;
                            }
                            r.drive_content_navigation()
                        };
                        let retry = if let Some(effect) = effect {
                            match effect(tw) {
                                Ok(false) => true,
                                outcome => {
                                    let mut render = render.lock();
                                    render.content_navigation.take();
                                    if matches!(outcome, Ok(true)) {
                                        render.refresh_coordinate_anchor();
                                    }
                                    if let Err(reason) = outcome {
                                        log::warn!("{reason}");
                                    }
                                    false
                                }
                            }
                        } else {
                            retry
                        };
                        let _ = send.send(retry);
                    })));
                    if !matches!(recv.await, Ok(true)) {
                        break;
                    }
                    sleep(Duration::from_millis(20)).await;
                }
            })
            .detach();
        true
    }

    fn drive_content_navigation(&mut self) -> (bool, Option<ContentEffect>) {
        let Some(mut pending) = self.content_navigation.take() else {
            return (false, None);
        };
        if !pending.matches(
            (self.cursor.x, self.cursor.y),
            self.start,
            self.selection_mode,
        ) {
            return (false, None);
        }
        let pane = Arc::clone(&self.delegate);
        let mut metadata = None;
        let result = pending.poll(
            &*pane,
            (self.cursor.x, self.cursor.y),
            self.start,
            self.selection_mode,
            &mut |x, authority, dimensions| {
                // Publication holds the pane's source/layout fence. Never call a
                // pane getter or selection helper that relocks the terminal here.
                self.cursor.x = x;
                metadata = Some((authority, dimensions));
            },
        );
        if let Some((authority, dims)) = metadata {
            let sequence = pending.source.unwrap().1;
            pending.cursor = (self.cursor.x, self.cursor.y);
            self.content_navigation = Some(pending);
            return (
                true,
                Some(self.finish_content_navigation(authority, sequence, dims)),
            );
        }
        if let Err(reason) = result {
            log::warn!("{reason}");
            self.window.invalidate();
            return (false, None);
        }
        self.content_navigation = Some(pending);
        (true, None)
    }

    fn finish_content_navigation(
        &self,
        authority: crate::selection::SelectionAuthority,
        sequence: SequenceNo,
        dims: RenderableDimensions,
    ) -> ContentEffect {
        let pane = Arc::clone(&self.delegate);
        let pane_id = pane.pane_id();
        let cursor = SelectionCoordinate::x_y(self.cursor.x, self.cursor.y);
        let mode = self.selection_mode;
        let range = self.start.map(|start| {
            if mode == SelectionMode::Line {
                let above = cursor.y < start.y;
                SelectionRange {
                    start: SelectionCoordinate::x_y(if above { usize::MAX } else { 0 }, start.y),
                    end: SelectionCoordinate::x_y(if above { 0 } else { usize::MAX }, cursor.y),
                }
            } else {
                SelectionRange { start, end: cursor }
            }
        });
        let top = self.viewport.unwrap_or(dims.physical_top);
        let gap: StableRowIndex = if dims.physical_top <= 5 { 1 } else { 5 };
        let viewport = if top > cursor.y || cursor.y.saturating_sub(top) < gap {
            Some(cursor.y.saturating_sub(gap))
        } else if StableRowIndex::try_from(dims.viewport_rows)
            .unwrap_or(StableRowIndex::MAX)
            .saturating_sub(cursor.y.saturating_sub(top))
            < gap
        {
            Some(cursor.y.saturating_add(gap).saturating_sub(
                StableRowIndex::try_from(dims.viewport_rows).unwrap_or(StableRowIndex::MAX),
            ))
        } else {
            None
        };
        let instance = Arc::clone(&self.instance_token);
        let action = Arc::clone(&self.content_action);
        let cursor_x = self.cursor.x;
        let selected_start = self.start;
        let publication = Arc::clone(&self.content_viewport_publication);
        self.window.invalidate();
        Box::new(move |tw| {
            // A queued completion cannot change a replacement overlay.
            let current = tw.pane_state(pane_id).and_then(|state| {
                state
                    .overlay
                    .as_ref()
                    .and_then(|overlay| overlay.pane.downcast_ref::<CopyOverlay>())
                    .map(|copy| {
                        let render = copy.render.lock();
                        Arc::ptr_eq(&render.instance_token, &instance)
                            && Arc::ptr_eq(&render.content_action, &action)
                            && render.cursor.x == cursor_x
                            && render.cursor.y == cursor.y
                            && render.start == selected_start
                            && render.selection_mode == mode
                    })
            });
            if current != Some(true) {
                return Err("copy content action superseded");
            }
            let Some((current_authority, current_sequence, current_dims)) =
                crate::selection::SelectionAuthority::capture_source(&*pane)
            else {
                return Ok(false);
            };
            if current_authority != authority
                || current_sequence != sequence
                || current_dims != dims
            {
                return Err("copy content source changed before selection/viewport publication");
            }
            if let Some(range) = range {
                tw.update_selection_with_seqno(&pane, Some(authority), sequence, |selection| {
                    selection.origin = Some(range.start);
                    selection.range = Some(range);
                    selection.rectangular = mode == SelectionMode::Block;
                });
            }
            if let Some(viewport) = viewport {
                publication.with_dimensions(dims, || {
                    tw.set_viewport(pane_id, Some(viewport), dims);
                });
            }
            Ok(true)
        })
    }

    fn move_to_end_of_line_content(&mut self) {
        if self.start_content_navigation(true) {
            return;
        }
        let y = self.cursor.y;
        let Some(line_range) = one_line_range(y) else {
            self.select_to_cursor_pos();
            return;
        };
        let (top, lines) = self.delegate.get_lines(line_range);
        if let Some(line) = lines.get(0) {
            self.cursor.y = top;
            self.cursor.x = 0;
            for cell in line.visible_cells() {
                if cell.str() != " " {
                    self.cursor.x = cell.cell_index();
                }
            }
        }
        self.select_to_cursor_pos();
    }

    fn move_to_start_of_line_content(&mut self) {
        if self.start_content_navigation(false) {
            return;
        }
        let y = self.cursor.y;
        let Some(line_range) = one_line_range(y) else {
            self.select_to_cursor_pos();
            return;
        };
        let (top, lines) = self.delegate.get_lines(line_range);
        if let Some(line) = lines.get(0) {
            self.cursor.y = top;
            self.cursor.x = 0;
            for cell in line.visible_cells() {
                if cell.str() != " " {
                    self.cursor.x = cell.cell_index();
                    break;
                }
            }
        }
        self.select_to_cursor_pos();
    }

    fn move_to_selection_other_end(&mut self) {
        if let Some(old_start) = self.start {
            // Swap cursor & start of selection
            self.start
                .replace(SelectionCoordinate::x_y(self.cursor.x, self.cursor.y));
            self.cursor.x = match &old_start.x {
                SelectionX::Cell(x) => *x,
                SelectionX::BeforeZero => 0,
            };
            self.cursor.y = old_start.y;
            self.select_to_cursor_pos();
        }
    }

    fn move_to_selection_other_end_horiz(&mut self) {
        if self.selection_mode != SelectionMode::Block {
            return self.move_to_selection_other_end();
        }
        if let Some(old_start) = self.start {
            // Swap X coordinate of cursor & start of selection
            self.start
                .replace(SelectionCoordinate::x_y(self.cursor.x, old_start.y));
            self.cursor.x = match &old_start.x {
                SelectionX::Cell(x) => *x,
                SelectionX::BeforeZero => 0,
            };
            self.select_to_cursor_pos();
        }
    }

    fn move_backward_one_word(&mut self) {
        let scrollback_top = self.delegate.get_dimensions().scrollback_top;
        let y = if self.cursor.x == 0 {
            if let Some(previous_row) =
                previous_row_within_scrollback(self.cursor.y, scrollback_top)
            {
                self.cursor.x = usize::MAX;
                previous_row
            } else {
                self.cursor.y
            }
        } else {
            self.cursor.y
        };

        let Some(line_range) = one_line_range(y) else {
            self.select_to_cursor_pos();
            return;
        };
        let (top, lines) = self.delegate.get_lines(line_range);
        if let Some(line) = lines.get(0) {
            self.cursor.y = top;
            if self.cursor.x == usize::MAX {
                self.cursor.x = line.len().saturating_sub(1);
            }
            let s = line.columns_as_str(0..self.cursor.x.saturating_add(1));

            // "hello there you"
            //              |_
            //        |    _
            //  |    _
            //        |     _
            //  |     _

            let mut last_was_whitespace = false;

            for (idx, word) in s.split_word_bounds().rev().enumerate() {
                let width = unicode_column_width(word, None);

                if is_whitespace_word(word) {
                    self.cursor.x = self.cursor.x.saturating_sub(width);
                    last_was_whitespace = true;
                    continue;
                }
                last_was_whitespace = false;

                if idx == 0 && width == 1 {
                    // We were at the start of the initial word
                    self.cursor.x = self.cursor.x.saturating_sub(width);
                    continue;
                }

                self.cursor.x = self.cursor.x.saturating_sub(width.saturating_sub(1));
                break;
            }

            if last_was_whitespace {
                // The line begins with whitespace
                if let Some(previous_row) =
                    previous_row_within_scrollback(self.cursor.y, scrollback_top)
                {
                    self.cursor.x = usize::MAX;
                    self.cursor.y = previous_row;
                    return self.move_backward_one_word();
                }
            }
        }
        self.select_to_cursor_pos();
    }

    fn move_forward_one_word(&mut self) {
        let y = self.cursor.y;
        let Some(line_range) = one_line_range(y) else {
            self.select_to_cursor_pos();
            return;
        };
        let (top, lines) = self.delegate.get_lines(line_range);
        if let Some(line) = lines.get(0) {
            self.cursor.y = top;
            let width = line.len();
            let s = line.columns_as_str(self.cursor.x..width.saturating_add(1));
            let mut words = s.split_word_bounds();

            if let Some(word) = words.next() {
                self.cursor.x = self
                    .cursor
                    .x
                    .saturating_add(unicode_column_width(word, None));
                if !is_whitespace_word(word) {
                    if let Some(word) = words.next() {
                        if is_whitespace_word(word) {
                            self.cursor.x = self
                                .cursor
                                .x
                                .saturating_add(unicode_column_width(word, None));
                        }
                    }
                }
            }

            if self.cursor.x >= width {
                let dims = self.delegate.get_dimensions();
                let next_row = self.cursor.y.checked_add(1);
                let max_row = checked_stable_row_end(dims.scrollback_top, dims.scrollback_rows);
                if let (Some(next_row), Some(max_row)) = (next_row, max_row) {
                    if next_row < max_row {
                        self.cursor.y = next_row;
                        return self.move_to_start_of_line_content();
                    }
                }
            }
        }
        self.select_to_cursor_pos();
    }

    fn move_to_end_of_word(&mut self) {
        let y = self.cursor.y;
        let Some(line_range) = one_line_range(y) else {
            self.select_to_cursor_pos();
            return;
        };
        let (top, lines) = self.delegate.get_lines(line_range);
        if let Some(line) = lines.get(0) {
            self.cursor.y = top;
            let width = line.len();
            let s = line.columns_as_str(self.cursor.x..width.saturating_add(1));
            let mut words = s.split_word_bounds();

            if self.cursor.x >= width.saturating_sub(1) {
                let dims = self.delegate.get_dimensions();
                let next_row = self.cursor.y.checked_add(1);
                let max_row = checked_stable_row_end(dims.scrollback_top, dims.scrollback_rows);
                if let (Some(next_row), Some(max_row)) = (next_row, max_row) {
                    if next_row < max_row {
                        self.cursor.y = next_row;
                        self.cursor.x = 0;
                        return self.move_to_end_of_word();
                    }
                }
            }

            if let Some(word) = words.next() {
                let mut word_end = self
                    .cursor
                    .x
                    .saturating_add(unicode_column_width(word, None));
                if !is_whitespace_word(word) {
                    if self.cursor.x == word_end.saturating_sub(1) {
                        while let Some(next_word) = words.next() {
                            word_end =
                                word_end.saturating_add(unicode_column_width(next_word, None));
                            if !is_whitespace_word(next_word) {
                                break;
                            }
                        }
                    }
                }
                while let Some(next_word) = words.next() {
                    if !is_whitespace_word(next_word) {
                        word_end = word_end.saturating_add(unicode_column_width(next_word, None));
                    } else {
                        break;
                    }
                }
                self.cursor.x = word_end.saturating_sub(1);
            }
        }
        self.select_to_cursor_pos();
    }

    fn move_by_zone(&mut self, mut delta: isize, zone_type: Option<SemanticType>) {
        if delta == 0 {
            return;
        }

        let zones = self
            .delegate
            .get_semantic_zones()
            .unwrap_or_else(|_| vec![]);
        let mut idx = match zones.binary_search_by(|zone| {
            if zone.start_y == self.cursor.y {
                zone.start_x.cmp(&self.cursor.x)
            } else if zone.start_y < self.cursor.y {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        }) {
            Ok(idx) | Err(idx) => idx,
        };

        let step = if delta > 0 { 1 } else { -1 };

        while delta != 0 {
            if step > 0 {
                idx = match idx.checked_add(1) {
                    Some(n) => n,
                    None => return,
                };
            } else {
                idx = match idx.checked_sub(1) {
                    Some(n) => n,
                    None => return,
                };
            }
            let zone = match zones.get(idx) {
                Some(z) => z,
                None => return,
            };
            if let Some(zone_type) = &zone_type {
                if zone.semantic_type != *zone_type {
                    continue;
                }
            }
            delta = delta.saturating_sub(step);

            self.cursor.x = zone.start_x;
            self.cursor.y = zone.start_y;
        }
        self.select_to_cursor_pos();
    }

    fn perform_jump(&mut self, jump: Jump, repeat: bool) {
        let y = self.cursor.y;
        let Some(line_range) = one_line_range(y) else {
            return;
        };
        let (_top, lines) = self.delegate.get_lines(line_range);
        let target_str = jump.target.to_string();
        if let Some(line) = lines.get(0) {
            // Find the indices of cells with a matching target
            let mut candidates: Vec<usize> = line
                .visible_cells()
                .filter_map(|cell| {
                    if cell.str() == &target_str {
                        Some(cell.cell_index())
                    } else {
                        None
                    }
                })
                .collect();

            if !jump.forward {
                candidates.reverse();
            }

            // Adjust cursor cutoff so that we don't end up matching
            // the current cursor position for the prev_char cases
            let cursor_x = match (jump.prev_char && repeat, jump.forward) {
                (false, _) => self.cursor.x,
                (true, true) => self.cursor.x.saturating_add(1),
                (true, false) => self.cursor.x.saturating_sub(1),
            };

            // Find the target that matches the jump
            let target = candidates
                .iter()
                .find(|&&idx| {
                    if jump.forward {
                        idx > cursor_x
                    } else {
                        idx < cursor_x
                    }
                })
                .copied();

            if let Some(target) = target {
                // We'll select the target cell index, or the cell
                // before/after depending on the prev_char and direction
                let target = match (jump.prev_char, jump.forward) {
                    (false, true | false) => target,
                    (true, true) => target.saturating_sub(1),
                    (true, false) => target.saturating_add(1),
                };

                self.cursor.x = target;
                self.select_to_cursor_pos();
            }
        }
    }

    fn jump(&mut self, forward: bool, prev_char: bool) {
        self.pending_jump
            .replace(PendingJump { forward, prev_char });
    }

    fn jump_again(&mut self, reverse: bool) {
        if let Some(mut jump) = self.last_jump {
            if reverse {
                jump.forward = !jump.forward;
            }
            self.perform_jump(jump, true);
        }
    }

    fn set_selection_mode(&mut self, mode: &Option<SelectionMode>) {
        match mode {
            None => self.clear_selection_mode(),
            Some(mode) => {
                if self.start.is_none() {
                    let coord = SelectionCoordinate::x_y(self.cursor.x, self.cursor.y);
                    self.start.replace(coord);
                } else if self.selection_mode == *mode {
                    // We have a selection and we're trying to set the same mode
                    // again; consider this to be a toggle that clears the selection
                    self.clear_selection_mode();
                    return;
                }
                self.selection_mode = *mode;
                self.select_to_cursor_pos();
            }
        }
    }

    fn clear_selection_mode(&mut self) {
        self.start.take();
        self.clear_selection();
    }
}

impl CopyOverlay {
    fn with_decorated_lines(
        &self,
        lines: Range<StableRowIndex>,
        hyperlink_rules: Option<&[termwiz::hyperlink::Rule]>,
        with_lines: &mut dyn WithPaneLines,
    ) {
        // Take care to access delegate methods before entering its callback;
        // the overlay renderer lock is intentionally held while applying the
        // copy-mode decorations to one coherent delegate snapshot.
        let mut renderer = self.render.lock();
        renderer.prepare_for_render(lines.clone());
        let dims = self.get_dimensions();
        let search_row = renderer.compute_search_row();

        struct OverlayLines<'a> {
            with_lines: &'a mut dyn WithPaneLines,
            dims: RenderableDimensions,
            search_row: StableRowIndex,
            renderer: &'a mut CopyRenderable,
        }

        impl WithPaneLines for OverlayLines<'_> {
            fn with_lines_mut(&mut self, first_row: StableRowIndex, lines: &mut [&mut Line]) {
                let mut overlay_lines = vec![];
                let config = config::configuration();
                let colors = &config.resolved_palette;

                for (idx, line) in lines.iter_mut().enumerate() {
                    let mut line: Line = line.clone();

                    let Some(stable_idx) = StableRowIndex::try_from(idx)
                        .ok()
                        .and_then(|offset| first_row.checked_add(offset))
                    else {
                        break;
                    };
                    let pattern = self.renderer.get_pattern();
                    if stable_idx == self.search_row
                        && (self.renderer.editing_search || !pattern.is_empty())
                    {
                        // Replace with search UI
                        let rev = CellAttributes::default().set_reverse(true).clone();
                        line.fill_range(0..self.dims.cols, &Cell::new(' ', rev.clone()), SEQ_ZERO);
                        let mode = &match pattern {
                            Pattern::CaseSensitiveString(_) => "case-sensitive",
                            Pattern::CaseInSensitiveString(_) => "ignore-case",
                            Pattern::Regex(_) => "regex",
                        };

                        let remain = match &self.renderer.searching {
                            Some(Searching { remain, .. }) => {
                                format!(" searching {remain} lines")
                            }
                            None if self.renderer.search_failed.is_some() => {
                                " search failed; edit query to retry".to_string()
                            }
                            None => String::new(),
                        };

                        line.overlay_text_with_attribute(
                            0,
                            &format!(
                                "Search: {} ({}/{} matches. {}{remain})",
                                *pattern,
                                self.renderer
                                    .result_pos
                                    .map(|x| x.saturating_add(1))
                                    .unwrap_or(0),
                                self.renderer.results.len(),
                                mode
                            ),
                            rev,
                            SEQ_ZERO,
                        );
                        self.renderer.last_bar_pos = Some(self.search_row);
                        line.clear_appdata();
                    } else if let Some(matches) = self.renderer.by_line.get(&stable_idx) {
                        for m in matches {
                            for cell_idx in m.range.clone() {
                                if let Some(cell) =
                                    line.cells_mut_for_attr_changes_only().get_mut(cell_idx)
                                {
                                    if Some(m.result_index) == self.renderer.result_pos {
                                        cell.attrs_mut()
                                            .set_background(
                                                colors
                                                    .copy_mode_active_highlight_bg
                                                    .unwrap_or(AnsiColor::Yellow.into()),
                                            )
                                            .set_foreground(
                                                colors
                                                    .copy_mode_active_highlight_fg
                                                    .unwrap_or(AnsiColor::Black.into()),
                                            )
                                            .set_reverse(false);
                                    } else {
                                        cell.attrs_mut()
                                            .set_background(
                                                colors
                                                    .copy_mode_inactive_highlight_bg
                                                    .unwrap_or(AnsiColor::Fuchsia.into()),
                                            )
                                            .set_foreground(
                                                colors
                                                    .copy_mode_inactive_highlight_fg
                                                    .unwrap_or(AnsiColor::Black.into()),
                                            )
                                            .set_reverse(false);
                                    }
                                }
                            }
                        }
                        line.clear_appdata();
                    }
                    overlay_lines.push(line);
                }

                // Decorated clones must not write their renderer appdata back
                // to the authoritative delegate lines: their cells differ.
                // Persisting shape caches across overlay reads therefore needs
                // an overlay-owned, revision-keyed cache rather than appdata
                // propagation through this copy-backed Pane API.
                let mut overlay_refs: Vec<&mut Line> = overlay_lines.iter_mut().collect();
                self.with_lines.with_lines_mut(first_row, &mut overlay_refs);
            }
        }

        let mut overlay = OverlayLines {
            with_lines,
            dims,
            search_row,
            renderer: &mut renderer,
        };
        if let Some(rules) = hyperlink_rules {
            self.delegate
                .with_lines_mut_and_apply_hyperlinks(lines, rules, &mut overlay);
        } else {
            self.delegate.with_lines_mut(lines, &mut overlay);
        }
    }
}

impl Pane for CopyOverlay {
    fn pane_id(&self) -> PaneId {
        self.delegate.pane_id()
    }

    fn mux_registration_slot(&self) -> &Arc<mux::PaneRegistrationSlot> {
        self.delegate.mux_registration_slot()
    }

    fn get_title(&self) -> String {
        format!("Copy mode: {}", self.delegate.get_title())
    }

    fn send_paste(&self, text: &str) -> anyhow::Result<()> {
        // paste into the search bar
        let mut r = self.render.lock();
        r.search_line.insert_text(text);
        r.schedule_update_search();
        Ok(())
    }

    fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
        Ok(None)
    }

    fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
        MutexGuard::map(self.writer.lock(), |writer| {
            let w: &mut dyn std::io::Write = writer;
            w
        })
    }

    fn resize(&self, _size: TerminalSize) -> anyhow::Result<()> {
        // The tab owns the delegate's geometry. Its resize is asynchronous,
        // so the overlay notification can still contain the previous size.
        // Forwarding it would replace the tab's newer resize request.
        Ok(())
    }

    fn key_up(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
        Ok(())
    }

    fn key_down(&self, key: KeyCode, mods: KeyModifiers) -> anyhow::Result<()> {
        let mut render = self.render.lock();
        let mods = mods.remove_positional_mods();
        if let Some(jump) = render.pending_jump.take() {
            match (key, mods) {
                (KeyCode::Char(c), KeyModifiers::NONE)
                | (KeyCode::Char(c), KeyModifiers::SHIFT) => {
                    let jump = Jump {
                        forward: jump.forward,
                        prev_char: jump.prev_char,
                        target: c,
                    };
                    render.last_jump.replace(jump);
                    render.perform_jump(jump, false);
                }
                _ => {
                    let registration = mux::Mux::get()
                        .capture_pane_registration(&self.delegate)
                        .ok_or_else(|| anyhow::anyhow!("terminal pane is no longer registered"))?;
                    mux::localpane::schedule_control_action(
                        registration,
                        mux::localpane::PaneControlAction::Bell,
                    )?;
                }
            }
            return Ok(());
        }

        if render.editing_search {
            match (key, mods) {
                (KeyCode::Char(c), KeyModifiers::NONE)
                | (KeyCode::Char(c), KeyModifiers::SHIFT) => {
                    // Type to add to the pattern
                    render.search_line.insert_char(c);

                    render.schedule_update_search();
                }
                (KeyCode::Char('H'), KeyModifiers::CTRL)
                | (KeyCode::Backspace, KeyModifiers::NONE) => {
                    render
                        .search_line
                        .kill_text(Movement::BackwardChar(1), Movement::BackwardChar(1));

                    render.schedule_update_search();
                }
                (KeyCode::Delete, KeyModifiers::NONE) => {
                    render
                        .search_line
                        .kill_text(Movement::ForwardChar(1), Movement::None);

                    render.schedule_update_search();
                }
                (KeyCode::Backspace, KeyModifiers::ALT)
                | (KeyCode::Char('W'), KeyModifiers::CTRL) => {
                    render
                        .search_line
                        .kill_text(Movement::BackwardWord(1), Movement::BackwardWord(1));

                    render.schedule_update_search();
                }
                (KeyCode::Backspace, KeyModifiers::SUPER) => {
                    render
                        .search_line
                        .kill_text(Movement::StartOfLine, Movement::StartOfLine);

                    render.schedule_update_search();
                }
                (KeyCode::Char('K'), KeyModifiers::CTRL) => {
                    render
                        .search_line
                        .kill_text(Movement::EndOfLine, Movement::EndOfLine);

                    render.schedule_update_search();
                }
                (KeyCode::Char('B'), KeyModifiers::CTRL)
                | (KeyCode::ApplicationLeftArrow, KeyModifiers::NONE)
                | (KeyCode::LeftArrow, KeyModifiers::NONE) => {
                    render.search_line.exec_movement(Movement::BackwardChar(1));
                }
                (KeyCode::Char('F'), KeyModifiers::CTRL)
                | (KeyCode::ApplicationRightArrow, KeyModifiers::NONE)
                | (KeyCode::RightArrow, KeyModifiers::NONE) => {
                    render.search_line.exec_movement(Movement::ForwardChar(1));
                }
                (KeyCode::ApplicationLeftArrow, KeyModifiers::CTRL)
                | (KeyCode::LeftArrow, KeyModifiers::CTRL) => {
                    render.search_line.exec_movement(Movement::BackwardWord(1));
                }
                (KeyCode::ApplicationRightArrow, KeyModifiers::CTRL)
                | (KeyCode::RightArrow, KeyModifiers::CTRL) => {
                    render.search_line.exec_movement(Movement::ForwardWord(1));
                }
                (KeyCode::Char('A'), KeyModifiers::CTRL) | (KeyCode::Home, KeyModifiers::NONE) => {
                    render.search_line.exec_movement(Movement::StartOfLine);
                }
                (KeyCode::Char('E'), KeyModifiers::CTRL) | (KeyCode::End, KeyModifiers::NONE) => {
                    render.search_line.exec_movement(Movement::EndOfLine);
                }
                _ => {}
            }
            render.mark_search_ui_dirty();
        }

        Ok(())
    }

    fn perform_assignment(&self, assignment: &KeyAssignment) -> PerformAssignmentResult {
        use CopyModeAssignment::*;
        let mut render = self.render.lock();
        if matches!(assignment, KeyAssignment::CopyMode(_))
            && !matches!(
                assignment,
                KeyAssignment::CopyMode(CopyModeAssignment::Close)
            )
            && !render.coordinates_current()
        {
            if render.coordinates_invalid || render.deferred_navigation.len() >= 16 {
                log::warn!(
                    "copy navigation refused: coordinate authority unavailable or pending queue full"
                );
            } else {
                render.deferred_navigation.push_back(assignment.clone());
                render.schedule_coordinate_driver();
            }
            return PerformAssignmentResult::Handled;
        }
        render.content_navigation.take();
        render.content_action = Arc::new(());
        if render.pending_jump.is_some() {
            // Block key assignments until key_down is called
            // and resolves the next state
            return PerformAssignmentResult::BlockAssignmentAndRouteToKeyDown;
        }
        match assignment {
            KeyAssignment::CopyMode(assignment) => {
                match assignment {
                    MoveToViewportBottom => render.move_to_viewport_bottom(),
                    MoveToViewportTop => render.move_to_viewport_top(),
                    MoveToViewportMiddle => render.move_to_viewport_middle(),
                    MoveToScrollbackTop => render.move_to_top(),
                    MoveToScrollbackBottom => render.move_to_bottom(),
                    MoveToStartOfLineContent => render.move_to_start_of_line_content(),
                    MoveToEndOfLineContent => render.move_to_end_of_line_content(),
                    MoveToStartOfLine => render.move_to_start_of_line(),
                    MoveToStartOfNextLine => render.move_to_start_of_next_line(),
                    MoveToSelectionOtherEnd => render.move_to_selection_other_end(),
                    MoveToSelectionOtherEndHoriz => render.move_to_selection_other_end_horiz(),
                    MoveBackwardWord => render.move_backward_one_word(),
                    MoveForwardWord => render.move_forward_one_word(),
                    MoveForwardWordEnd => render.move_to_end_of_word(),
                    MoveRight => render.move_right_single_cell(),
                    MoveLeft => render.move_left_single_cell(),
                    MoveUp => render.move_up_single_row(),
                    MoveDown => render.move_down_single_row(),
                    MoveByPage(n) => render.move_by_page(**n),
                    PageUp => render.move_by_page(-1.0),
                    PageDown => render.move_by_page(1.0),
                    Close => render.close(),
                    PriorMatch => render.prior_match(),
                    NextMatch => render.next_match(),
                    PriorMatchPage => render.prior_match_page(),
                    NextMatchPage => render.next_match_page(),
                    CycleMatchType => render.cycle_match_type(),
                    ClearPattern => render.clear_pattern(),
                    EditPattern => render.edit_pattern(),
                    AcceptPattern => render.accept_pattern(),
                    SetSelectionMode(mode) => render.set_selection_mode(mode),
                    ClearSelectionMode => render.clear_selection_mode(),
                    MoveBackwardSemanticZone => render.move_by_zone(-1, None),
                    MoveForwardSemanticZone => render.move_by_zone(1, None),
                    MoveBackwardZoneOfType(zone_type) => render.move_by_zone(-1, Some(*zone_type)),
                    MoveForwardZoneOfType(zone_type) => render.move_by_zone(1, Some(*zone_type)),
                    JumpForward { prev_char } => render.jump(true, *prev_char),
                    JumpBackward { prev_char } => render.jump(false, *prev_char),
                    JumpAgain => render.jump_again(false),
                    JumpReverse => render.jump_again(true),
                }
                PerformAssignmentResult::Handled
            }
            _ => PerformAssignmentResult::Unhandled,
        }
    }

    fn mouse_event(&self, _event: MouseEvent) -> anyhow::Result<()> {
        anyhow::bail!("ignoring mouse while copying");
    }

    fn perform_actions(
        &self,
        actions: Vec<termwiz::escape::Action>,
    ) -> Result<(), mux::pane::PaneActionAdmissionError> {
        self.delegate.perform_actions(actions)
    }

    fn is_dead(&self) -> bool {
        self.delegate.is_dead()
    }

    fn palette(&self) -> ColorPalette {
        self.delegate.palette()
    }

    fn domain_id(&self) -> DomainId {
        self.delegate.domain_id()
    }

    fn erase_scrollback(&self, erase_mode: ScrollbackEraseMode) {
        self.delegate.erase_scrollback(erase_mode)
    }

    fn is_mouse_grabbed(&self) -> bool {
        // Force grabbing off while we're searching
        false
    }

    fn is_alt_screen_active(&self) -> bool {
        false
    }

    fn set_clipboard(&self, clipboard: &Arc<dyn Clipboard>) {
        self.delegate.set_clipboard(clipboard)
    }

    fn get_current_working_dir(&self, policy: CachePolicy) -> Option<Url> {
        self.delegate.get_current_working_dir(policy)
    }

    fn get_cursor_position(&self) -> StableCursorPosition {
        let renderer = self.render.lock();
        if renderer.editing_search {
            // place in the search box
            // Padding between the start of the editable line and the left side of the terminal
            const SEARCH_CURSOR_PADDING: usize = 8;
            let cursor = unicode_column_width(
                &renderer.search_line.get_line()[0..renderer.search_line.get_cursor()],
                None,
            );
            StableCursorPosition {
                x: SEARCH_CURSOR_PADDING.saturating_add(cursor),
                y: renderer.compute_search_row(),
                shape: termwiz::surface::CursorShape::SteadyBlock,
                visibility: termwiz::surface::CursorVisibility::Visible,
            }
        } else {
            let mut cursor = renderer.cursor;
            if !renderer.coordinates_current() {
                cursor.visibility = CursorVisibility::Hidden;
            }
            cursor
        }
    }

    fn get_current_seqno(&self) -> SequenceNo {
        self.delegate.get_current_seqno()
    }

    fn get_render_cache_generation(&self) -> u64 {
        self.delegate.get_render_cache_generation()
    }

    fn get_changed_since(
        &self,
        lines: Range<StableRowIndex>,
        seqno: SequenceNo,
    ) -> RangeSet<StableRowIndex> {
        let dirty = self.delegate.get_changed_since(lines.clone(), seqno);
        merge_dirty_results(lines, dirty, &self.render.lock().dirty_results)
    }

    fn get_changed_since_with_source_fence(
        &self,
        lines: Range<StableRowIndex>,
        last_observed_source_end: SequenceNo,
    ) -> (SequenceNo, RangeSet<StableRowIndex>) {
        // Preserve the delegate's atomic post-poll fence. Falling back to the
        // Pane default would sample current-seq before ClientPane polls and then
        // perform a second, separately locked changed-row query.
        let (source_end, dirty) = self
            .delegate
            .get_changed_since_with_source_fence(lines.clone(), last_observed_source_end);
        let mut renderer = self.render.lock();
        let retained = retained_row_range(renderer.delegate.get_dimensions());
        prune_dirty_results(&mut renderer.dirty_results, retained);
        (
            source_end,
            take_dirty_results(lines, dirty, &mut renderer.dirty_results),
        )
    }

    fn get_logical_lines(&self, lines: Range<StableRowIndex>) -> Vec<LogicalLine> {
        self.delegate.get_logical_lines(lines)
    }

    fn for_each_logical_line_in_stable_range_mut(
        &self,
        lines: Range<StableRowIndex>,
        for_line: &mut dyn ForEachPaneLogicalLine,
    ) {
        self.delegate
            .for_each_logical_line_in_stable_range_mut(lines, for_line);
    }

    fn with_lines_mut(&self, lines: Range<StableRowIndex>, with_lines: &mut dyn WithPaneLines) {
        self.with_decorated_lines(lines, None, with_lines);
    }

    fn with_lines_mut_and_apply_hyperlinks(
        &self,
        lines: Range<StableRowIndex>,
        rules: &[termwiz::hyperlink::Rule],
        with_lines: &mut dyn WithPaneLines,
    ) {
        self.with_decorated_lines(lines, Some(rules), with_lines);
    }

    fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
        let mut renderer = self.render.lock();
        renderer.prepare_for_render(lines.clone());
        let dims = self.get_dimensions();

        let (top, mut lines) = self.delegate.get_lines(lines);

        let config = config::configuration();
        let colors = &config.resolved_palette;

        // Process the lines; for the search row we want to render instead
        // the search UI.
        // For rows with search results, we want to highlight the matching ranges
        let search_row = renderer.compute_search_row();
        for (idx, line) in lines.iter_mut().enumerate() {
            let Some(stable_idx) = StableRowIndex::try_from(idx)
                .ok()
                .and_then(|offset| top.checked_add(offset))
            else {
                break;
            };
            let pattern = renderer.get_pattern();
            if stable_idx == search_row && (renderer.editing_search || !pattern.is_empty()) {
                // Replace with search UI
                let rev = CellAttributes::default().set_reverse(true).clone();
                line.fill_range(0..dims.cols, &Cell::new(' ', rev.clone()), SEQ_ZERO);
                let mode = &match pattern {
                    Pattern::CaseSensitiveString(_) => "case-sensitive",
                    Pattern::CaseInSensitiveString(_) => "ignore-case",
                    Pattern::Regex(_) => "regex",
                };
                line.overlay_text_with_attribute(
                    0,
                    &format!(
                        "Search: {} ({}/{} matches. {})",
                        *pattern,
                        renderer
                            .result_pos
                            .map(|x| x.saturating_add(1))
                            .unwrap_or(0),
                        renderer.results.len(),
                        mode
                    ),
                    rev,
                    SEQ_ZERO,
                );
                renderer.last_bar_pos = Some(search_row);
            } else if let Some(matches) = renderer.by_line.get(&stable_idx) {
                for m in matches {
                    // highlight
                    for cell_idx in m.range.clone() {
                        if let Some(cell) = line.cells_mut_for_attr_changes_only().get_mut(cell_idx)
                        {
                            if Some(m.result_index) == renderer.result_pos {
                                cell.attrs_mut()
                                    .set_background(
                                        colors
                                            .copy_mode_active_highlight_bg
                                            .unwrap_or(AnsiColor::Yellow.into()),
                                    )
                                    .set_foreground(
                                        colors
                                            .copy_mode_active_highlight_fg
                                            .unwrap_or(AnsiColor::Black.into()),
                                    )
                                    .set_reverse(false);
                            } else {
                                cell.attrs_mut()
                                    .set_background(
                                        colors
                                            .copy_mode_inactive_highlight_bg
                                            .unwrap_or(AnsiColor::Fuchsia.into()),
                                    )
                                    .set_foreground(
                                        colors
                                            .copy_mode_inactive_highlight_fg
                                            .unwrap_or(AnsiColor::Black.into()),
                                    )
                                    .set_reverse(false);
                            }
                        }
                    }
                }
            }
        }

        (top, lines)
    }

    fn get_dimensions(&self) -> RenderableDimensions {
        self.delegate.get_dimensions()
    }
}

pub struct SearchOverlayPatternWriter {
    render: Arc<Mutex<CopyRenderable>>,
}

impl std::io::Write for SearchOverlayPatternWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut render = self.render.lock();
        let s = std::str::from_utf8(buf).map_err(|err| {
            std::io::Error::new(std::io::ErrorKind::Other, format!("invalid UTF-8: {err:#}"))
        })?;
        render.search_line.insert_text(s);
        render.schedule_update_search();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn is_whitespace_word(word: &str) -> bool {
    if let Some(c) = word.chars().next() {
        c.is_whitespace()
    } else {
        false
    }
}

pub fn search_key_table() -> KeyTable {
    let mut table = KeyTable::default();
    for (key, mods, action) in [
        (
            WKeyCode::Char('\x1b'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::Close),
        ),
        (
            WKeyCode::UpArrow,
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::PriorMatch),
        ),
        (
            WKeyCode::Char('\r'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::PriorMatch),
        ),
        (
            WKeyCode::Char('p'),
            Modifiers::CTRL,
            KeyAssignment::CopyMode(CopyModeAssignment::PriorMatch),
        ),
        (
            WKeyCode::PageUp,
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::PriorMatchPage),
        ),
        (
            WKeyCode::PageDown,
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::NextMatchPage),
        ),
        (
            WKeyCode::Char('n'),
            Modifiers::CTRL,
            KeyAssignment::CopyMode(CopyModeAssignment::NextMatch),
        ),
        (
            WKeyCode::DownArrow,
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::NextMatch),
        ),
        (
            WKeyCode::Char('r'),
            Modifiers::CTRL,
            KeyAssignment::CopyMode(CopyModeAssignment::CycleMatchType),
        ),
        (
            WKeyCode::Char('u'),
            Modifiers::CTRL,
            KeyAssignment::CopyMode(CopyModeAssignment::ClearPattern),
        ),
    ] {
        table.insert((key, mods), KeyTableEntry { action });
    }
    table
}

fn scroll_to_bottom_and_close() -> KeyAssignment {
    KeyAssignment::Multiple(vec![
        KeyAssignment::ScrollToBottom,
        KeyAssignment::CopyMode(CopyModeAssignment::Close),
    ])
}

pub fn copy_key_table() -> KeyTable {
    let mut table = KeyTable::default();
    for (key, mods, action) in [
        (
            WKeyCode::Char('c'),
            Modifiers::CTRL,
            scroll_to_bottom_and_close(),
        ),
        (
            WKeyCode::Char('g'),
            Modifiers::CTRL,
            scroll_to_bottom_and_close(),
        ),
        (
            WKeyCode::Char('q'),
            Modifiers::NONE,
            scroll_to_bottom_and_close(),
        ),
        (
            WKeyCode::Char('\x1b'),
            Modifiers::NONE,
            scroll_to_bottom_and_close(),
        ),
        (
            WKeyCode::Char('h'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveLeft),
        ),
        (
            WKeyCode::LeftArrow,
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveLeft),
        ),
        (
            WKeyCode::Char('j'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveDown),
        ),
        (
            WKeyCode::DownArrow,
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveDown),
        ),
        (
            WKeyCode::Char('k'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveUp),
        ),
        (
            WKeyCode::UpArrow,
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveUp),
        ),
        (
            WKeyCode::Char('l'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveRight),
        ),
        (
            WKeyCode::RightArrow,
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveRight),
        ),
        (
            WKeyCode::RightArrow,
            Modifiers::ALT,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveForwardWord),
        ),
        (
            WKeyCode::Char('f'),
            Modifiers::ALT,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveForwardWord),
        ),
        (
            WKeyCode::Char('\t'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveForwardWord),
        ),
        (
            WKeyCode::Char('w'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveForwardWord),
        ),
        (
            WKeyCode::Char('e'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveForwardWordEnd),
        ),
        (
            WKeyCode::LeftArrow,
            Modifiers::ALT,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveBackwardWord),
        ),
        (
            WKeyCode::Char('b'),
            Modifiers::ALT,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveBackwardWord),
        ),
        (
            WKeyCode::Char('\t'),
            Modifiers::SHIFT,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveBackwardWord),
        ),
        (
            WKeyCode::Char('b'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveBackwardWord),
        ),
        (
            WKeyCode::Char('0'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToStartOfLine),
        ),
        (
            WKeyCode::Char('\r'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToStartOfNextLine),
        ),
        (
            WKeyCode::Char('$'),
            Modifiers::SHIFT,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToEndOfLineContent),
        ),
        (
            WKeyCode::Char('$'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToEndOfLineContent),
        ),
        (
            WKeyCode::Char('m'),
            Modifiers::ALT,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToStartOfLineContent),
        ),
        (
            WKeyCode::Char('^'),
            Modifiers::SHIFT,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToStartOfLineContent),
        ),
        (
            WKeyCode::Char('^'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToStartOfLineContent),
        ),
        (
            WKeyCode::Char(' '),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::SetSelectionMode(Some(
                SelectionMode::Cell,
            ))),
        ),
        (
            WKeyCode::Char('v'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::SetSelectionMode(Some(
                SelectionMode::Cell,
            ))),
        ),
        (
            WKeyCode::Char('V'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::SetSelectionMode(Some(
                SelectionMode::Line,
            ))),
        ),
        (
            WKeyCode::Char('V'),
            Modifiers::SHIFT,
            KeyAssignment::CopyMode(CopyModeAssignment::SetSelectionMode(Some(
                SelectionMode::Line,
            ))),
        ),
        (
            WKeyCode::Char('v'),
            Modifiers::CTRL,
            KeyAssignment::CopyMode(CopyModeAssignment::SetSelectionMode(Some(
                SelectionMode::Block,
            ))),
        ),
        (
            WKeyCode::Char('G'),
            Modifiers::SHIFT,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToScrollbackBottom),
        ),
        (
            WKeyCode::Char('G'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToScrollbackBottom),
        ),
        (
            WKeyCode::Char('g'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToScrollbackTop),
        ),
        (
            WKeyCode::Char('H'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToViewportTop),
        ),
        (
            WKeyCode::Char('H'),
            Modifiers::SHIFT,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToViewportTop),
        ),
        (
            WKeyCode::Char('M'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToViewportMiddle),
        ),
        (
            WKeyCode::Char('M'),
            Modifiers::SHIFT,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToViewportMiddle),
        ),
        (
            WKeyCode::Char('L'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToViewportBottom),
        ),
        (
            WKeyCode::Char('L'),
            Modifiers::SHIFT,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToViewportBottom),
        ),
        (
            WKeyCode::PageUp,
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::PageUp),
        ),
        (
            WKeyCode::PageDown,
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::PageDown),
        ),
        (
            WKeyCode::Char('b'),
            Modifiers::CTRL,
            KeyAssignment::CopyMode(CopyModeAssignment::PageUp),
        ),
        (
            WKeyCode::Char('f'),
            Modifiers::CTRL,
            KeyAssignment::CopyMode(CopyModeAssignment::PageDown),
        ),
        (
            WKeyCode::Char('u'),
            Modifiers::CTRL,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveByPage(NotNan::new(-0.5).unwrap())),
        ),
        (
            WKeyCode::Char('d'),
            Modifiers::CTRL,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveByPage(NotNan::new(0.5).unwrap())),
        ),
        (
            WKeyCode::Char('o'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToSelectionOtherEnd),
        ),
        (
            WKeyCode::Char('O'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToSelectionOtherEndHoriz),
        ),
        (
            WKeyCode::Char('O'),
            Modifiers::SHIFT,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToSelectionOtherEndHoriz),
        ),
        (
            WKeyCode::Char('y'),
            Modifiers::NONE,
            KeyAssignment::Multiple(vec![
                KeyAssignment::CopyTo(ClipboardCopyDestination::ClipboardAndPrimarySelection),
                scroll_to_bottom_and_close(),
            ]),
        ),
        (
            WKeyCode::Char(';'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::JumpAgain),
        ),
        (
            WKeyCode::Char(','),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::JumpReverse),
        ),
        (
            WKeyCode::Char('F'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::JumpBackward { prev_char: false }),
        ),
        (
            WKeyCode::Char('F'),
            Modifiers::SHIFT,
            KeyAssignment::CopyMode(CopyModeAssignment::JumpBackward { prev_char: false }),
        ),
        (
            WKeyCode::Char('T'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::JumpBackward { prev_char: true }),
        ),
        (
            WKeyCode::Char('T'),
            Modifiers::SHIFT,
            KeyAssignment::CopyMode(CopyModeAssignment::JumpBackward { prev_char: true }),
        ),
        (
            WKeyCode::Char('f'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::JumpForward { prev_char: false }),
        ),
        (
            WKeyCode::Char('t'),
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::JumpForward { prev_char: true }),
        ),
        (
            WKeyCode::Home,
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToStartOfLine),
        ),
        (
            WKeyCode::End,
            Modifiers::NONE,
            KeyAssignment::CopyMode(CopyModeAssignment::MoveToEndOfLineContent),
        ),
    ] {
        table.insert((key, mods), KeyTableEntry { action });
    }
    table
}
