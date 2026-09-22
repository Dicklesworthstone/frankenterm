// The range_plus_one lint can't see when the LHS is not compatible with
// and inclusive range
#![allow(clippy::range_plus_one)]
use frankenterm_core::smart_selection::SelectionPatternKind;
use frankenterm_core::smart_selection_patterns::{smart_match_at_click, smart_match_in_line};
use mux::pane::{LineReadPermit, LogicalLine, Pane};
use std::cmp::Ordering;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{self, AtomicBool};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use termwiz::surface::SequenceNo;
use termwiz::surface::line::DoubleClickRange;
use wezterm_term::screen::ScreenLineRead;
use wezterm_term::unicode_column_width;
use wezterm_term::{SemanticZone, StableRowIndex};

/// Result of a successful smart-selection pick. Carries the pattern
/// kind plus the selected text so the GUI mouse handler can emit
/// the matching `SmartSelectionA11yMessage` to the AT-tree without
/// re-borrowing the line text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmartSelectionPick {
    pub kind: SelectionPatternKind,
    pub text: String,
}

#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub struct Selection {
    /// Remembers the starting coordinate of the selection prior to
    /// dragging.
    pub origin: Option<SelectionCoordinate>,
    /// Holds the not-normalized selection range.
    pub range: Option<SelectionRange>,
    /// When the selection was made wrt. the pane content
    pub seqno: SequenceNo,
    /// Authority of the coordinates, independent of the drag's damage seqno.
    pub authority: Option<SelectionAuthority>,
    /// Whether the selection is rectangular
    pub rectangular: bool,
    native_anchor: Option<NativeSelectionAnchor>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct NativeSelectionAnchor {
    token: wezterm_term::screen::ScreenSelectionAnchor,
    authority: Option<SelectionAuthority>,
    origin: Option<SelectionCoordinate>,
    range: Option<SelectionRange>,
}

/// One uncommitted native gesture. Contention must neither replace the last
/// anchored selection nor discard the final endpoint when the button lifts.
#[derive(Debug)]
pub(crate) struct PendingNativeSelection {
    pub desired: Selection,
    pub copy: Option<config::keyassignment::ClipboardCopyDestination>,
    pub paint_retries_remaining: u8,
    pub committed: bool,
    pub text_copy: Option<crate::termwindow::SelectionCopy>,
}

pub(crate) enum NativeSelectionCapture {
    Ready(wezterm_term::screen::ScreenSelectionAnchor),
    Unremappable,
    Busy,
    Invalidated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeSelectionCommit {
    Applied { needs_repaint: bool },
    Invalidated,
}

impl
    From<
        Result<
            Option<wezterm_term::screen::ScreenSelectionAnchor>,
            mux::localpane::SelectionAnchorCaptureError,
        >,
    > for NativeSelectionCapture
{
    fn from(
        result: Result<
            Option<wezterm_term::screen::ScreenSelectionAnchor>,
            mux::localpane::SelectionAnchorCaptureError,
        >,
    ) -> Self {
        match result {
            Ok(Some(token)) => Self::Ready(token),
            Ok(None) => Self::Unremappable,
            Err(mux::localpane::SelectionAnchorCaptureError::Busy) => Self::Busy,
            Err(mux::localpane::SelectionAnchorCaptureError::SourceChanged) => Self::Invalidated,
        }
    }
}

impl PendingNativeSelection {
    pub(crate) fn expire_text_copy(pending: &mut Option<Self>, now: std::time::Instant) -> bool {
        if pending
            .as_ref()
            .and_then(|pending| pending.text_copy.as_ref())
            .is_some_and(|copy| now >= copy.deadline())
        {
            *pending = None;
            true
        } else {
            false
        }
    }

    pub fn new(desired: Selection) -> Self {
        Self {
            desired,
            copy: None,
            paint_retries_remaining: 3,
            committed: false,
            text_copy: None,
        }
    }

    /// None retains the intent; Invalidated retires stale coordinates; Applied commits
    /// only after the source has registered an anchor or explicitly established
    /// current coordinates that cannot be remapped (such as alternate screen).
    pub fn try_commit(
        &self,
        committed: &mut Selection,
        capture: NativeSelectionCapture,
    ) -> Option<NativeSelectionCommit> {
        let mut next = self.desired.clone();
        match capture {
            NativeSelectionCapture::Busy => return None,
            NativeSelectionCapture::Invalidated => return Some(NativeSelectionCommit::Invalidated),
            NativeSelectionCapture::Unremappable => {
                next.native_anchor = None;
            }
            NativeSelectionCapture::Ready(token) => {
                next.remember_native_anchor(token);
            }
        }
        let needs_repaint = committed.origin != next.origin
            || committed.range != next.range
            || committed.authority != next.authority
            || committed.rectangular != next.rectangular;
        *committed = next;
        Some(NativeSelectionCommit::Applied { needs_repaint })
    }

    pub fn take_paint_retry(&mut self) -> bool {
        let Some(remaining) = self.paint_retries_remaining.checked_sub(1) else {
            return false;
        };
        self.paint_retries_remaining = remaining;
        true
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct SelectionAuthority {
    source: usize,
    sequence: SequenceNo,
    geometry: (usize, usize, u32, usize, usize),
    alternate: bool,
}

impl SelectionAuthority {
    pub fn from_native_frame(
        pane: &dyn Pane,
        frame: &mux::localpane::NativeRenderFrame,
    ) -> Option<Self> {
        Self::from_native_snapshot(pane, frame.layout_floor, frame.dimensions)
    }

    pub(crate) fn from_native_snapshot(
        pane: &dyn Pane,
        floor: SequenceNo,
        dims: mux::renderable::RenderableDimensions,
    ) -> Option<Self> {
        if floor == SequenceNo::MAX {
            return None;
        }
        Some(Self {
            source: pane as *const dyn Pane as *const () as usize,
            sequence: floor,
            geometry: (
                dims.cols,
                dims.viewport_rows,
                dims.dpi,
                dims.pixel_width,
                dims.pixel_height,
            ),
            alternate: false,
        })
    }

    pub fn capture(pane: &dyn Pane) -> Option<Self> {
        Self::capture_source(pane).map(|(authority, _, _)| authority)
    }

    pub(crate) fn layout_floor(self) -> SequenceNo {
        self.sequence
    }

    pub fn capture_source(
        pane: &dyn Pane,
    ) -> Option<(Self, SequenceNo, mux::renderable::RenderableDimensions)> {
        // Native layout capture is atomic and nonblocking. Busy must never
        // fall back to a fabricated stamp from separately sampled metadata.
        let (sequence, source_sequence, dims, alternate) = if let Some(local) =
            pane.downcast_ref::<mux::localpane::LocalPane>()
        {
            let (floor, source_sequence, dims) = local.selection_source_snapshot()?;
            (floor, source_sequence, dims, false) // The native floor includes Screen identity.
        } else if let Some(client) = pane.downcast_ref::<frankenterm_client::pane::ClientPane>() {
            client.selection_source_snapshot()?
        } else {
            // Other backends do not expose a layout floor yet. Conservatively
            // bind to their content sequence rather than disabling selection.
            let before = pane.get_current_seqno();
            let dims = pane.get_dimensions();
            let alternate = pane.is_alt_screen_active();
            if pane.get_current_seqno() != before {
                return None;
            }
            (before, before, dims, alternate)
        };
        if sequence == SequenceNo::MAX {
            return None;
        }
        Some((
            Self {
                source: pane as *const dyn Pane as *const () as usize,
                sequence,
                geometry: (
                    dims.cols,
                    dims.viewport_rows,
                    dims.dpi,
                    dims.pixel_width,
                    dims.pixel_height,
                ),
                alternate,
            },
            source_sequence,
            dims,
        ))
    }
}

/// Coordinates of a fully populated pane in a submitted frame. This is a
/// synchronous presentation boundary, not a GPU-completion/scanout receipt.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct SelectionFrameStamp {
    pub authority: SelectionAuthority,
    pub source_sequence: SequenceNo,
    pub viewport: StableRowIndex,
    pub geometry: [usize; 12],
}

impl SelectionFrameStamp {
    pub fn same_coordinates(self, other: Self) -> bool {
        self.authority == other.authority
            && self.viewport == other.viewport
            && self.geometry == other.geometry
    }
}

pub type SelectionReadPlans = anyhow::Result<Vec<ScreenLineRead>>;

pub struct WordLineSelectionReady {
    pub result: Option<anyhow::Result<WordLineSelectionPayload>>,
    pub plans: Option<SelectionReadPlans>,
    pub retire: SyncSender<SelectionReadPlans>,
}

impl Drop for WordLineSelectionReady {
    fn drop(&mut self) {
        if let Some(plans) = self.plans.take() {
            let _ = self.retire.send(plans);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WordLineSelectionPayload {
    pub range: SelectionRange,
    pub pick: Option<SmartSelectionPick>,
}

pub struct WordLineSelectionRead {
    receiver: Receiver<WordLineSelectionReady>,
    ready: Option<WordLineSelectionReady>,
    cancelled: Arc<AtomicBool>,
    deadline: std::time::Instant,
    source_sequence: SequenceNo,
    authority: SelectionAuthority,
    mode: SelectionMode,
    coordinate: SelectionCoordinate,
    end_coordinate: Option<SelectionCoordinate>,
}

impl Drop for WordLineSelectionRead {
    fn drop(&mut self) {
        self.cancelled.store(true, atomic::Ordering::Release);
    }
}

impl std::fmt::Debug for WordLineSelectionRead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WordLineSelectionRead")
            .field("deadline", &self.deadline)
            .field("source_sequence", &self.source_sequence)
            .field("authority", &self.authority)
            .field("mode", &self.mode)
            .field("coordinate", &self.coordinate)
            .field("end_coordinate", &self.end_coordinate)
            .field("ready", &self.ready.is_some())
            .finish_non_exhaustive()
    }
}

impl WordLineSelectionRead {
    pub const FIXED_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);
    pub const MAX_CONTEXT_ROWS: usize = 64;

    #[must_use]
    pub fn deadline(&self) -> std::time::Instant {
        self.deadline
    }

    #[must_use]
    pub fn source_sequence(&self) -> SequenceNo {
        self.source_sequence
    }

    #[must_use]
    pub fn authority(&self) -> SelectionAuthority {
        self.authority
    }

    #[must_use]
    pub fn mode(&self) -> SelectionMode {
        self.mode
    }

    #[must_use]
    pub fn coordinate(&self) -> SelectionCoordinate {
        self.coordinate
    }

    #[must_use]
    pub fn end_coordinate(&self) -> Option<SelectionCoordinate> {
        self.end_coordinate
    }

    pub fn start(
        pane: &Arc<dyn Pane>,
        authority: SelectionAuthority,
        sequence: SequenceNo,
        mode: SelectionMode,
        coordinate: SelectionCoordinate,
        end_coordinate: Option<SelectionCoordinate>,
        deadline: std::time::Instant,
        wake: impl FnOnce() + Send + 'static,
    ) -> Result<Option<Self>, &'static str> {
        if std::time::Instant::now() >= deadline {
            return Err("The selection deadline expired. Select again.");
        }
        let Some(permit) = LineReadPermit::try_acquire() else {
            return Ok(None);
        };

        let start_y = coordinate.y;
        let start_req = start_y.saturating_sub(Self::MAX_CONTEXT_ROWS as StableRowIndex)
            ..start_y.saturating_add(Self::MAX_CONTEXT_ROWS as StableRowIndex + 1);

        let mut requested_ranges = vec![start_req];
        if let Some(end) = end_coordinate {
            let end_y = end.y;
            let in_start_req =
                end_y >= requested_ranges[0].start && end_y < requested_ranges[0].end;
            if !in_start_req {
                let end_req = end_y.saturating_sub(Self::MAX_CONTEXT_ROWS as StableRowIndex)
                    ..end_y.saturating_add(Self::MAX_CONTEXT_ROWS as StableRowIndex + 1);
                requested_ranges.push(end_req);
            }
        }

        let cancelled = Arc::new(AtomicBool::new(false));
        let is_cancelled = Arc::clone(&cancelled);
        let exec_cancelled = Arc::clone(&cancelled);
        let (sender, receiver) = sync_channel(1);
        let ranges_for_worker = requested_ranges.clone();

        // Reserve and start before capturing any source allocations.
        let worker = permit
            .start(
                move || {
                    is_cancelled.load(atomic::Ordering::Acquire)
                        || std::time::Instant::now() >= deadline
                },
                move |plans, permit| {
                    let (retire, retired) = sync_channel(1);
                    let result = process_hydrated_word_line_selection(
                        &plans,
                        &ranges_for_worker,
                        mode,
                        coordinate,
                        end_coordinate,
                        &exec_cancelled,
                    );
                    let ready = WordLineSelectionReady {
                        result: Some(result),
                        plans: Some(plans),
                        retire,
                    };
                    if sender.send(ready).is_ok() {
                        wake();
                        drop(retired.recv());
                    }
                    drop(permit);
                },
            )
            .map_err(|_| "The text reader could not start. Select again.")?;

        let mut submitted_plans = Vec::with_capacity(requested_ranges.len());
        let mut capture_budget = Default::default();
        for range in &requested_ranges {
            let capture = pane.capture_line_read(range.clone(), &mut capture_budget);
            let Some(plan) = capture else {
                cancelled.store(true, atomic::Ordering::Release);
                worker.submit(submitted_plans);
                return Err("This pane cannot provide a bounded text read.");
            };
            let Ok(plan) = plan else {
                cancelled.store(true, atomic::Ordering::Release);
                worker.submit(submitted_plans);
                return Ok(None);
            };
            submitted_plans.push(plan);
        }

        worker.submit(submitted_plans);

        Ok(Some(Self {
            receiver,
            ready: None,
            cancelled,
            deadline,
            source_sequence: sequence,
            authority,
            mode,
            coordinate,
            end_coordinate,
        }))
    }

    pub fn poll_ready(&mut self) -> Result<bool, &'static str> {
        if std::time::Instant::now() >= self.deadline {
            self.cancelled.store(true, atomic::Ordering::Release);
            return Err("The selection deadline expired. Select again.");
        }
        if self.ready.is_some() {
            return Ok(true);
        }
        match self.receiver.try_recv() {
            Ok(ready) => {
                self.ready = Some(ready);
                Ok(true)
            }
            Err(TryRecvError::Empty) => Ok(false),
            Err(TryRecvError::Disconnected) => {
                Err("The text reader stopped unexpectedly. Select again.")
            }
        }
    }

    pub fn plans(&self) -> Option<&[ScreenLineRead]> {
        self.ready
            .as_ref()?
            .plans
            .as_ref()?
            .as_ref()
            .ok()
            .map(|p| p.as_slice())
    }

    pub fn take_payload(&mut self) -> Result<WordLineSelectionPayload, &'static str> {
        let ready = self.ready.as_mut().ok_or("Selection read is not ready")?;
        let result = ready
            .result
            .take()
            .ok_or("Selection result already consumed")?;
        self.ready = None;
        match result {
            Ok(payload) => Ok(payload),
            Err(_) => Err("Incomplete selection snapshot. Select again."),
        }
    }

    #[cfg(test)]
    pub fn try_take_result(&mut self) -> Result<Option<WordLineSelectionPayload>, &'static str> {
        if !self.poll_ready()? {
            return Ok(None);
        }
        self.take_payload().map(Some)
    }
}

fn logical_lines_from_plan(
    plan: &ScreenLineRead,
    requested: &Range<StableRowIndex>,
    target_coord: SelectionCoordinate,
    cancelled: &Arc<AtomicBool>,
) -> anyhow::Result<(Vec<LogicalLine>, usize)> {
    let row_count = plan.row_count();
    anyhow::ensure!(row_count > 0, "empty line read snapshot");
    let first_row = plan.first_row();
    let last_snapshot_row = first_row
        .checked_add(row_count as StableRowIndex - 1)
        .ok_or_else(|| anyhow::anyhow!("row overflow"))?;

    let mut logical_lines = Vec::new();
    for (idx, line) in plan.lines().enumerate() {
        if cancelled.load(atomic::Ordering::Acquire) {
            anyhow::bail!("selection read cancelled");
        }
        let row = first_row
            .checked_add(idx as StableRowIndex)
            .ok_or_else(|| anyhow::anyhow!("row overflow"))?;
        match logical_lines.last_mut() {
            None => {
                logical_lines.push(LogicalLine {
                    physical_lines: vec![line.clone()],
                    logical: line.clone(),
                    first_row: row,
                });
            }
            Some(prior)
                if prior.logical.last_cell_was_wrapped()
                    && !prior
                        .logical
                        .len()
                        .checked_add(line.len())
                        .map_or(true, |n| n > mux::pane::MAX_LOGICAL_LINE_LEN) =>
            {
                let seqno = prior.logical.current_seqno().max(line.current_seqno());
                prior.logical.set_last_cell_was_wrapped(false, seqno);
                prior.logical.append_line(line.clone(), seqno);
                prior.physical_lines.push(line.clone());
            }
            Some(_) => {
                logical_lines.push(LogicalLine {
                    physical_lines: vec![line.clone()],
                    logical: line.clone(),
                    first_row: row,
                });
            }
        }
    }

    let (target_idx, target_logical) = logical_lines
        .iter()
        .enumerate()
        .find(|(_, ll)| ll.contains_y(target_coord.y))
        .ok_or_else(|| anyhow::anyhow!("target coordinate not contained in line snapshot"))?;

    // Decline incomplete snapshots: verify logical line was not truncated at the boundary
    if let Some(last_phys) = target_logical.physical_lines.last() {
        let last_phys_row = target_logical
            .first_row
            .checked_add(target_logical.physical_lines.len() as StableRowIndex - 1)
            .ok_or_else(|| anyhow::anyhow!("row overflow"))?;
        if last_phys_row == last_snapshot_row && last_phys.last_cell_was_wrapped() {
            anyhow::bail!("logical line truncated at forward snapshot boundary");
        }
    }

    // Decline backward truncated logical lines that extend to snapshot boundary
    if target_logical.first_row == first_row
        && first_row == requested.start
        && !plan.starts_at_known_history_start()
    {
        anyhow::bail!("logical line truncated at backward snapshot boundary");
    }

    Ok((logical_lines, target_idx))
}

fn process_hydrated_word_line_selection(
    plans: &anyhow::Result<Vec<ScreenLineRead>>,
    requested_ranges: &[Range<StableRowIndex>],
    mode: SelectionMode,
    start_coord: SelectionCoordinate,
    end_coord: Option<SelectionCoordinate>,
    cancelled: &Arc<AtomicBool>,
) -> anyhow::Result<WordLineSelectionPayload> {
    anyhow::ensure!(
        !cancelled.load(atomic::Ordering::Acquire),
        "selection read cancelled"
    );
    let plans = plans
        .as_ref()
        .map_err(|e| anyhow::anyhow!("line read failed: {e}"))?;
    anyhow::ensure!(
        plans.len() == requested_ranges.len(),
        "incomplete line read plans"
    );
    anyhow::ensure!(!plans.is_empty(), "missing line read plans");

    let (start_lines, _) =
        logical_lines_from_plan(&plans[0], &requested_ranges[0], start_coord, cancelled)?;

    let (mut range, mut pick) = match mode {
        SelectionMode::Word => {
            SelectionRange::smart_or_word_around_in_logical_lines(start_coord, &start_lines)
        }
        SelectionMode::Line => {
            SelectionRange::smart_or_line_around_in_logical_lines(start_coord, &start_lines)
        }
        _ => anyhow::bail!("unsupported mode for background selection reader"),
    };

    if let Some(end) = end_coord {
        if end != start_coord {
            let (end_lines, _) = if requested_ranges.len() > 1 {
                logical_lines_from_plan(&plans[1], &requested_ranges[1], end, cancelled)?
            } else {
                logical_lines_from_plan(&plans[0], &requested_ranges[0], end, cancelled)?
            };
            let (end_range, end_pick) = match mode {
                SelectionMode::Word => {
                    SelectionRange::smart_or_word_around_in_logical_lines(end, &end_lines)
                }
                SelectionMode::Line => {
                    SelectionRange::smart_or_line_around_in_logical_lines(end, &end_lines)
                }
                _ => anyhow::bail!("unsupported mode"),
            };
            range = range.extend_with(end_range);
            if end_pick.is_some() {
                pick = end_pick;
            }
        }
    }

    Ok(WordLineSelectionPayload { range, pick })
}

#[derive(Debug)]
pub struct SelectionStartDeadline(pub futures::future::AbortHandle);

impl Drop for SelectionStartDeadline {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A press whose displayed coordinates are known, but whose source is busy.
#[derive(Debug, Clone)]
pub struct PendingSelectionStart {
    pub frame: SelectionFrameStamp,
    pub coordinate: SelectionCoordinate,
    pub mode: SelectionMode,
    pub button: window::MousePress,
    pub paint_retries_remaining: u8,
    pub released: bool,
    pub end: Option<(wezterm_term::input::ClickPosition, StableRowIndex)>,
    pub copy: Option<config::keyassignment::ClipboardCopyDestination>,
    pub read: Option<Arc<parking_lot::Mutex<WordLineSelectionRead>>>,
    pub deadline: std::time::Instant,
    pub deadline_wake: Option<Arc<SelectionStartDeadline>>,
}

impl PendingSelectionStart {
    pub fn new(
        frame: SelectionFrameStamp,
        coordinate: SelectionCoordinate,
        mode: SelectionMode,
        button: window::MousePress,
    ) -> Self {
        Self {
            frame,
            coordinate,
            mode,
            button,
            paint_retries_remaining: 3,
            released: false,
            end: None,
            copy: None,
            read: None,
            deadline: std::time::Instant::now() + WordLineSelectionRead::FIXED_DEADLINE,
            deadline_wake: None,
        }
    }

    #[must_use]
    pub fn is_expired(&self) -> bool {
        std::time::Instant::now() >= self.deadline
    }

    pub fn retain_endpoint(
        &mut self,
        frame: SelectionFrameStamp,
        position: wezterm_term::input::ClickPosition,
        row: StableRowIndex,
        release: Option<window::MousePress>,
    ) {
        let final_release = release == Some(self.button);
        if (!self.released || final_release) && frame.same_coordinates(self.frame) {
            self.end = Some((position, row));
        }
        if final_release {
            self.released = true;
        }
    }

    pub fn take_paint_retry(&mut self) -> bool {
        let Some(remaining) = self.paint_retries_remaining.checked_sub(1) else {
            return false;
        };
        self.paint_retries_remaining = remaining;
        true
    }

    /// Defer a busy/unpresented frame and retire obsolete pixel coordinates.
    pub fn resolve(
        &self,
        current: Option<SelectionFrameStamp>,
        frames: &SelectionFrameState,
    ) -> PendingSelectionResolution {
        let Some(current) = current else {
            return PendingSelectionResolution::Wait;
        };
        if !self.frame.same_coordinates(current) {
            return PendingSelectionResolution::Invalidated;
        }
        if frames.for_mouse(Some(current)).is_some() {
            PendingSelectionResolution::Ready
        } else {
            PendingSelectionResolution::Wait
        }
    }
}

#[derive(Debug)]
pub enum PendingSelectionResolution {
    Wait,
    Ready,
    Invalidated,
}

/// Hyperlink hit targets from the rule-expanded lines actually drawn. Keeping a
/// weak pane reference prevents a reused pane ID (or address) from inheriting
/// another incarnation's links without keeping a retired pane alive.
#[derive(Debug)]
pub(crate) struct FrameHyperlinks {
    pane: std::sync::Weak<dyn Pane>,
    registration: Option<mux::PaneRegistrationHandle>,
    rows: Vec<(StableRowIndex, Vec<HyperlinkSpan>)>,
    double_height_bottom: Option<(StableRowIndex, Vec<HyperlinkSpan>)>,
}

/// Window-pixel hit interval emitted by the glyph layout used for drawing.
/// Cell indices cannot represent bidi ordering, double-width lines, ligatures,
/// or proportional positioning. Mouse movement must never reshape the source.
#[derive(Clone, Debug)]
pub(crate) struct HyperlinkSpan {
    left: f32,
    right: f32,
    link: Arc<termwiz::hyperlink::Hyperlink>,
}

pub(crate) fn retain_hyperlink_span(
    spans: &mut Vec<HyperlinkSpan>,
    link: Option<&Arc<termwiz::hyperlink::Hyperlink>>,
    occupied: Range<f32>,
    clip: Range<f32>,
) {
    let Some(link) = link else { return };
    if !occupied.start.is_finite()
        || !occupied.end.is_finite()
        || !clip.start.is_finite()
        || !clip.end.is_finite()
    {
        return;
    }
    let left = occupied.start.max(clip.start);
    let right = occupied.end.min(clip.end);
    if left < right {
        if let Some(previous) = spans.last_mut() {
            if Arc::ptr_eq(&previous.link, link) && left <= previous.right && previous.left <= right
            {
                previous.left = previous.left.min(left);
                previous.right = previous.right.max(right);
                return;
            }
        }
        spans.push(HyperlinkSpan {
            left,
            right,
            link: Arc::clone(link),
        });
    }
}

impl FrameHyperlinks {
    pub(crate) fn new(pane: &Arc<dyn Pane>) -> Self {
        Self {
            pane: Arc::downgrade(pane),
            registration: pane.mux_registration_slot().load(),
            rows: Vec::new(),
            double_height_bottom: None,
        }
    }

    pub(crate) fn retain_line(
        &mut self,
        row: StableRowIndex,
        mut spans: Vec<HyperlinkSpan>,
        double_height_top: bool,
        double_height_bottom: bool,
    ) {
        let preceding_top = self.double_height_bottom.take();
        if double_height_bottom {
            // The renderer skips this row because the preceding top row drew
            // both halves. Never invent targets if that top was offscreen.
            spans = preceding_top
                .filter(|(expected, _)| *expected == row)
                .map(|(_, spans)| spans)
                .unwrap_or_default();
        }
        if double_height_top {
            self.double_height_bottom = row.checked_add(1).map(|next| (next, spans.clone()));
        }
        self.rows.push((row, spans));
    }

    fn link_at(
        &self,
        pane: &Arc<dyn Pane>,
        row: StableRowIndex,
        pixel_x: f32,
    ) -> Option<Arc<termwiz::hyperlink::Hyperlink>> {
        let current = pane.mux_registration_slot().load();
        let same_registration = match (&self.registration, &current) {
            (Some(expected), Some(current)) => {
                expected.same_registration(current) && expected.try_with_current(|_| ()).is_some()
            }
            // Unregistered embedded panes have allocation identity only. A
            // later registration must never inherit their displayed targets.
            (None, None) => true,
            _ => false,
        };
        if !same_registration {
            return None;
        }
        if !self
            .pane
            .upgrade()
            .is_some_and(|owner| Arc::ptr_eq(&owner, pane))
        {
            return None;
        }
        let index = self.rows.binary_search_by_key(&row, |(row, _)| *row).ok()?;
        self.rows[index]
            .1
            .iter()
            .find(|span| pixel_x >= span.left && pixel_x < span.right)
            .map(|span| Arc::clone(&span.link))
    }
}

#[derive(Debug, Default)]
pub struct SelectionFrameState {
    pending: Option<SelectionFrameStamp>,
    presented: Option<SelectionFrameStamp>,
    pending_hyperlinks: Option<FrameHyperlinks>,
    presented_hyperlinks: Option<FrameHyperlinks>,
}

impl SelectionFrameState {
    pub fn displayed_for_geometry(&self, geometry: [usize; 12]) -> Option<SelectionFrameStamp> {
        self.presented.filter(|frame| frame.geometry == geometry)
    }
    pub fn begin_attempt(&mut self) {
        self.pending = None;
        self.pending_hyperlinks = None;
    }
    pub fn stage(
        &mut self,
        before: Option<SelectionFrameStamp>,
        after: Option<SelectionFrameStamp>,
        complete: bool,
    ) {
        self.pending = before
            .filter(|before| complete && after.is_some_and(|after| before.same_coordinates(after)));
    }
    /// Return only the complete stamp promoted by this successful submission.
    /// An omitted/incomplete pane must not report its previously displayed stamp.
    pub fn presented(&mut self) -> Option<SelectionFrameStamp> {
        self.presented = self.pending.take();
        self.presented_hyperlinks = self
            .pending_hyperlinks
            .take()
            .filter(|_| self.presented.is_some());
        self.presented
    }
    pub(crate) fn stage_hyperlinks(&mut self, links: FrameHyperlinks) {
        self.pending_hyperlinks = self.pending.map(|_| links);
    }
    pub(crate) fn hyperlink_at(
        &self,
        pane: &Arc<dyn Pane>,
        frame: SelectionFrameStamp,
        row: StableRowIndex,
        pixel_x: f32,
    ) -> Option<Arc<termwiz::hyperlink::Hyperlink>> {
        if self.presented != Some(frame) {
            return None;
        }
        self.presented_hyperlinks
            .as_ref()?
            .link_at(pane, row, pixel_x)
    }
    pub fn for_mouse(&self, current: Option<SelectionFrameStamp>) -> Option<SelectionFrameStamp> {
        self.presented
            .filter(|presented| current.is_some_and(|current| presented.same_coordinates(current)))
    }
}

pub use config::keyassignment::SelectionMode;

impl Selection {
    pub(crate) fn native_points(
        &self,
    ) -> [Option<wezterm_term::screen::SelectionAnchorCoordinate>; 3] {
        [
            self.origin,
            self.range.map(|r| r.start),
            self.range.map(|r| r.end),
        ]
        .map(|point| {
            point.map(|point| wezterm_term::screen::SelectionAnchorCoordinate {
                row: point.y,
                column: match point.x {
                    SelectionX::Cell(column) => Some(column),
                    SelectionX::BeforeZero => None,
                },
            })
        })
    }

    pub(crate) fn remember_native_anchor(
        &mut self,
        token: wezterm_term::screen::ScreenSelectionAnchor,
    ) {
        self.native_anchor = Some(NativeSelectionAnchor {
            token,
            authority: self.authority,
            origin: self.origin,
            range: self.range,
        });
    }

    pub(crate) fn native_anchor(&self) -> Option<&wezterm_term::screen::ScreenSelectionAnchor> {
        self.native_anchor
            .as_ref()
            .filter(|anchor| {
                !self.rectangular
                    && anchor.authority == self.authority
                    && anchor.origin == self.origin
                    && anchor.range == self.range
            })
            .map(|anchor| &anchor.token)
    }

    /// Only the exact selection that captured a native token can consume its
    /// remap. Later mouse motion must not be overwritten by prepared work.
    pub(crate) fn rebase_native_anchor(
        &mut self,
        points: [Option<wezterm_term::screen::SelectionAnchorCoordinate>; 3],
        authority: SelectionAuthority,
        source_sequence: SequenceNo,
    ) -> bool {
        let Some(token) = self.native_anchor().cloned() else {
            return false;
        };
        if source_sequence == SequenceNo::MAX
            || self.origin.is_some() != points[0].is_some()
            || self.range.is_some() != points[1].is_some()
            || self.range.is_some() != points[2].is_some()
        {
            return false;
        }
        let [origin, start, end] = points.map(|point| {
            point.map(|point| SelectionCoordinate {
                x: point
                    .column
                    .map_or(SelectionX::BeforeZero, SelectionX::Cell),
                y: point.row,
            })
        });
        self.origin = origin;
        self.range = start
            .zip(end)
            .map(|(start, end)| SelectionRange { start, end });
        self.authority = Some(authority);
        self.seqno = source_sequence;
        self.remember_native_anchor(token);
        true
    }

    /// An unavailable nonblocking snapshot is not evidence of a new layout.
    /// Keep the anchor until it can be checked; reads still require authority.
    pub fn is_invalidated_by(&self, current: Option<SelectionAuthority>) -> bool {
        current.is_some() && self.authority.is_some() && self.authority != current
    }

    pub fn is_authorized_by(&self, current: Option<SelectionAuthority>) -> bool {
        self.authority.is_some() && self.authority == current
    }
    pub fn clear(&mut self) {
        self.range = None;
        self.origin = None;
        self.authority = None;
        self.native_anchor = None;
    }

    pub fn begin(&mut self, origin: SelectionCoordinate) {
        self.native_anchor = None;
        self.range = None;
        self.origin = Some(origin);
    }

    pub fn is_empty(&self) -> bool {
        self.range.is_none()
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum SelectionX {
    /// Zero-based cell index
    Cell(usize),
    /// Exactly before the 0th cell
    BeforeZero,
}

impl SelectionX {
    pub const fn saturating_add(self, rhs: usize) -> Self {
        match self {
            Self::Cell(x) => Self::Cell(x.saturating_add(rhs)),
            Self::BeforeZero => {
                if rhs == 0 {
                    Self::BeforeZero
                } else {
                    Self::Cell(rhs - 1)
                }
            }
        }
    }

    pub const fn saturating_sub(self, rhs: usize) -> Self {
        match self {
            Self::Cell(x) => match x.checked_sub(rhs) {
                Some(x) => Self::Cell(x),
                None => Self::BeforeZero,
            },
            Self::BeforeZero => Self::BeforeZero,
        }
    }

    pub const fn range(self, rhs: Self) -> Range<usize> {
        match self {
            Self::Cell(left) => match rhs {
                Self::Cell(right) => left..right,
                Self::BeforeZero => 0..0,
            },
            Self::BeforeZero => match rhs {
                Self::Cell(right) => 0..right,
                Self::BeforeZero => 0..0,
            },
        }
    }
}

impl Default for SelectionX {
    // Default is 0th cell
    fn default() -> Self {
        Self::Cell(0)
    }
}

impl PartialEq<usize> for SelectionX {
    fn eq(&self, other: &usize) -> bool {
        match self {
            Self::Cell(x) => x == other,
            _ => false,
        }
    }
}

impl PartialEq<SelectionX> for usize {
    fn eq(&self, other: &SelectionX) -> bool {
        other == self
    }
}

impl Ord for SelectionX {
    fn cmp(&self, other: &Self) -> Ordering {
        match self {
            Self::Cell(x1) => match other {
                Self::Cell(x2) => x1.cmp(x2),
                Self::BeforeZero => Ordering::Greater,
            },
            Self::BeforeZero => match other {
                Self::Cell(_) => Ordering::Less,
                Self::BeforeZero => Ordering::Equal,
            },
        }
    }
}

impl PartialOrd for SelectionX {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialOrd<usize> for SelectionX {
    fn partial_cmp(&self, other: &usize) -> Option<Ordering> {
        self.partial_cmp(&Self::Cell(*other))
    }
}

impl PartialOrd<SelectionX> for usize {
    fn partial_cmp(&self, other: &SelectionX) -> Option<Ordering> {
        SelectionX::Cell(*self).partial_cmp(other)
    }
}

/// The x,y coordinates of either the start or end of a selection region
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq)]
pub struct SelectionCoordinate {
    pub x: SelectionX,
    pub y: StableRowIndex,
}

impl SelectionCoordinate {
    pub const fn x_y(x: usize, y: StableRowIndex) -> Self {
        Self {
            x: SelectionX::Cell(x),
            y,
        }
    }
}

/// Represents the selected text range.
/// The end coordinates are inclusive.
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq)]
pub struct SelectionRange {
    pub start: SelectionCoordinate,
    pub end: SelectionCoordinate,
}

fn is_double_click_word(s: &str) -> bool {
    match s.chars().count() {
        1 => !config::configuration().selection_word_boundary.contains(s),
        0 => false,
        _ => true,
    }
}

fn byte_offset_for_logical_x(text: &str, logical_x: usize) -> usize {
    let mut display_x: usize = 0;
    for (byte_offset, ch) in text.char_indices() {
        let next_byte_offset = byte_offset + ch.len_utf8();
        let width = unicode_column_width(&text[byte_offset..next_byte_offset], None);
        let next_display_x = display_x.saturating_add(width);
        if logical_x < next_display_x {
            return byte_offset;
        }
        display_x = next_display_x;
    }
    text.len()
}

fn logical_x_for_byte_offset(text: &str, byte_offset: usize) -> usize {
    if byte_offset >= text.len() {
        return unicode_column_width(text, None);
    }

    let Some(prefix) = text.get(..byte_offset) else {
        return unicode_column_width(text, None);
    };
    unicode_column_width(prefix, None)
}

/// Smart-match result that carries both the GUI-side display range
/// (logical-x columns, post-unicode-width translation) and the
/// pattern kind + selected text needed to emit a
/// `SmartSelectionA11yMessage`.
struct SmartLogicalMatch {
    range: Range<usize>,
    kind: SelectionPatternKind,
    text: String,
}

fn smart_match_logical_x_range(text: &str, click_logical_x: usize) -> Option<SmartLogicalMatch> {
    let click_byte_offset = byte_offset_for_logical_x(text, click_logical_x);
    let smart_match = smart_match_at_click(text, click_byte_offset)?;
    let start = logical_x_for_byte_offset(text, smart_match.span_start);
    let end_exclusive = logical_x_for_byte_offset(text, smart_match.span_end);
    if start >= end_exclusive {
        return None;
    }
    let selected_text = text
        .get(smart_match.span_start..smart_match.span_end)?
        .to_string();
    Some(SmartLogicalMatch {
        range: start..end_exclusive,
        kind: smart_match.kind,
        text: selected_text,
    })
}

impl SelectionRange {
    fn from_logical_click_range(
        start: SelectionCoordinate,
        logical: &mux::pane::LogicalLine,
        click_range: Range<usize>,
    ) -> Self {
        if click_range.is_empty() {
            return Self { start, end: start };
        }

        let (start_y, start_x) = logical.logical_x_to_physical_coord(click_range.start);
        let (end_y, end_x) = logical.logical_x_to_physical_coord(click_range.end - 1);
        Self {
            start: SelectionCoordinate::x_y(start_x, start_y),
            end: SelectionCoordinate::x_y(end_x, end_y),
        }
    }

    /// Create a new range that starts at the specified location
    pub fn start(start: SelectionCoordinate) -> Self {
        let end = start;
        Self { start, end }
    }

    fn line_around_in_logical_lines(
        start: SelectionCoordinate,
        lines: &[mux::pane::LogicalLine],
    ) -> Self {
        for logical in lines {
            if logical.contains_y(start.y) {
                let offset = (logical.physical_lines.len().saturating_sub(1)) as StableRowIndex;
                let end_row = logical.first_row.saturating_add(offset);
                return Self {
                    start: SelectionCoordinate::x_y(0, logical.first_row),
                    end: SelectionCoordinate::x_y(usize::MAX, end_row),
                };
            }
        }
        Self { start, end: start }
    }

    /// Computes the selection range for the line around the specified coords
    pub fn line_around(start: SelectionCoordinate, pane: &dyn Pane) -> Self {
        let Some(end_y) = start.y.checked_add(1) else {
            return Self { start, end: start };
        };
        let lines = pane.get_logical_lines(start.y..end_y);
        Self::line_around_in_logical_lines(start, &lines)
    }

    pub fn zone_around(start: SelectionCoordinate, pane: &dyn mux::pane::Pane) -> Self {
        let zones = match pane.get_semantic_zones() {
            Ok(z) => z,
            Err(_) => return Self { start, end: start },
        };

        fn find_zone(start: &SelectionCoordinate, zone: &SemanticZone) -> Ordering {
            match zone.start_y.cmp(&start.y) {
                Ordering::Greater => return Ordering::Greater,
                // If the zone starts on the same line then check that the
                // x position is within bounds
                Ordering::Equal => match SelectionX::Cell(zone.start_x).cmp(&start.x) {
                    Ordering::Greater => return Ordering::Greater,
                    Ordering::Equal | Ordering::Less => {}
                },
                Ordering::Less => {}
            }
            match zone.end_y.cmp(&start.y) {
                Ordering::Less => Ordering::Less,
                // If the zone ends on the same line then check that the
                // x position is within bounds
                Ordering::Equal => match SelectionX::Cell(zone.end_x).cmp(&start.x) {
                    Ordering::Less => Ordering::Less,
                    Ordering::Equal | Ordering::Greater => Ordering::Equal,
                },
                Ordering::Greater => Ordering::Equal,
            }
        }

        if let Ok(idx) = zones.binary_search_by(|zone| find_zone(&start, zone)) {
            let zone = &zones[idx];
            Self {
                start: SelectionCoordinate::x_y(zone.start_x, zone.start_y),
                end: SelectionCoordinate::x_y(zone.end_x, zone.end_y),
            }
        } else {
            Self { start, end: start }
        }
    }

    fn word_around_in_logical_lines(
        start: SelectionCoordinate,
        lines: &[mux::pane::LogicalLine],
    ) -> Self {
        for logical in lines {
            if !logical.contains_y(start.y) {
                continue;
            }

            if let SelectionX::Cell(start_x) = start.x {
                let start_idx = logical.xy_to_logical_x(start_x, start.y);
                return match logical
                    .logical
                    .compute_double_click_range(start_idx, is_double_click_word)
                {
                    DoubleClickRange::RangeWithWrap(click_range)
                    | DoubleClickRange::Range(click_range) => {
                        Self::from_logical_click_range(start, logical, click_range)
                    }
                };
            }
        }

        Self { start, end: start }
    }

    /// Computes the selection range for the word around the specified coords
    pub fn word_around(start: SelectionCoordinate, pane: &dyn Pane) -> Self {
        let Some(end_y) = start.y.checked_add(1) else {
            return Self { start, end: start };
        };
        let lines = pane.get_logical_lines(start.y..end_y);
        Self::word_around_in_logical_lines(start, &lines)
    }

    fn smart_or_word_around_in_logical_lines(
        start: SelectionCoordinate,
        lines: &[mux::pane::LogicalLine],
    ) -> (Self, Option<SmartSelectionPick>) {
        for logical in lines {
            if !logical.contains_y(start.y) {
                continue;
            }

            if let SelectionX::Cell(start_x) = start.x {
                let click_logical_x = logical.xy_to_logical_x(start_x, start.y);
                let line_text = logical.logical.as_str();
                if let Some(smart) = smart_match_logical_x_range(&line_text, click_logical_x) {
                    let (start_y, start_x) = logical.logical_x_to_physical_coord(smart.range.start);
                    let (end_y, end_x) =
                        logical.logical_x_to_physical_coord(smart.range.end.saturating_sub(1));
                    let range = Self {
                        start: SelectionCoordinate::x_y(start_x, start_y),
                        end: SelectionCoordinate::x_y(end_x, end_y),
                    };
                    return (
                        range,
                        Some(SmartSelectionPick {
                            kind: smart.kind,
                            text: smart.text,
                        }),
                    );
                }
            }
        }

        (Self::word_around_in_logical_lines(start, lines), None)
    }

    /// Computes the smart-selection range for the specified coords,
    /// falling back to the legacy word-boundary selection when the
    /// smart-selection catalog has no match at the click position.
    ///
    /// Reads logical line snapshots from the pane once, ensuring the
    /// same snapshot is used for both smart pattern evaluation and
    /// word-boundary fallback without a second pane read.
    ///
    /// Returns the resolved range plus a `Some(SmartSelectionPick)`
    /// when a smart pattern was matched (so the caller can emit the
    /// AT-tree announcement) or `None` when the word-boundary
    /// fallback fired (avoids screen-reader noise on plain word
    /// picks per ft-cnil8.4 acceptance).
    pub fn smart_or_word_around(
        start: SelectionCoordinate,
        pane: &dyn Pane,
    ) -> (Self, Option<SmartSelectionPick>) {
        let Some(end_y) = start.y.checked_add(1) else {
            return (Self { start, end: start }, None);
        };
        let lines = pane.get_logical_lines(start.y..end_y);
        Self::smart_or_word_around_in_logical_lines(start, &lines)
    }

    fn smart_or_line_around_in_logical_lines(
        start: SelectionCoordinate,
        lines: &[mux::pane::LogicalLine],
    ) -> (Self, Option<SmartSelectionPick>) {
        for logical in lines {
            if !logical.contains_y(start.y) {
                continue;
            }

            let line_text = logical.logical.as_str();
            // Triple-click resolves to the widest smart-pattern fully
            // contained within the line. `smart_match_in_line` runs
            // find_all → drop_shell_quoted_supersets →
            // classify_triple_click in one call.
            if let Some(selection_match) = smart_match_in_line(&line_text, 0, line_text.len()) {
                let logical_start =
                    logical_x_for_byte_offset(&line_text, selection_match.span_start);
                let logical_end = logical_x_for_byte_offset(&line_text, selection_match.span_end);
                if logical_start < logical_end {
                    let (start_y, start_x) = logical.logical_x_to_physical_coord(logical_start);
                    let (end_y, end_x) =
                        logical.logical_x_to_physical_coord(logical_end.saturating_sub(1));
                    if let Some(text) =
                        line_text.get(selection_match.span_start..selection_match.span_end)
                    {
                        let range = Self {
                            start: SelectionCoordinate::x_y(start_x, start_y),
                            end: SelectionCoordinate::x_y(end_x, end_y),
                        };
                        return (
                            range,
                            Some(SmartSelectionPick {
                                kind: selection_match.kind,
                                text: text.to_string(),
                            }),
                        );
                    }
                }
            }
        }

        (Self::line_around_in_logical_lines(start, lines), None)
    }

    /// Computes the smart-selection range for the *line* around the
    /// specified coords (triple-click semantics), falling back to
    /// the legacy full-physical-line selection when the smart-
    /// selection catalog has no widest-pattern fully contained in
    /// the line.
    ///
    /// Reads logical line snapshots from the pane once, ensuring the
    /// same snapshot is used for both smart pattern evaluation and
    /// line fallback without a second pane read.
    ///
    /// Returns the resolved range plus a `Some(SmartSelectionPick)`
    /// when a smart pattern was matched in the line — so the caller
    /// can emit the AT-tree announcement — or `None` when the
    /// legacy line fallback fired (avoids screen-reader noise on
    /// plain line picks, per ft-cnil8.4 acceptance).
    ///
    /// br-ft-t5j0a: closes the ghostwiring gap left by ft-cnil8.2's
    /// substrate-only closure. The substrate
    /// (`smart_match_in_line` at c1cfbb435 + `classify_triple_click`
    /// at 668e8d662) was unreachable from the GUI mouse handler
    /// until this function landed.
    pub fn smart_or_line_around(
        start: SelectionCoordinate,
        pane: &dyn Pane,
    ) -> (Self, Option<SmartSelectionPick>) {
        let Some(end_y) = start.y.checked_add(1) else {
            return (Self { start, end: start }, None);
        };
        let lines = pane.get_logical_lines(start.y..end_y);
        Self::smart_or_line_around_in_logical_lines(start, &lines)
    }

    /// Extends the current selection by unioning it with another selection range
    pub fn extend_with(&self, other: Self) -> Self {
        let norm = self.normalize();
        let other = other.normalize();
        let (start, end) = if (norm.start.y < other.start.y)
            || (norm.start.y == other.start.y && norm.start.x <= other.start.x)
        {
            (norm, other)
        } else {
            (other, norm)
        };
        Self {
            start: start.start,
            end: end.end,
        }
    }

    /// Returns an extended selection that it ends at the specified location
    pub fn extend(&self, end: SelectionCoordinate) -> Self {
        Self {
            start: self.start,
            end,
        }
    }

    /// Return a normalized selection such that the starting y coord
    /// is <= the ending y coord.
    pub fn normalize(&self) -> Self {
        if self.start.y <= self.end.y {
            *self
        } else {
            Self {
                start: self.end,
                end: self.start,
            }
        }
    }

    /// Yields a range representing the row indices.
    /// Make sure that you invoke this on a normalized range!
    pub fn rows(&self) -> Range<StableRowIndex> {
        let norm = self.normalize();
        norm.start.y..norm.end.y + 1
    }

    /// Yields a range representing the selected columns for the specified row.
    /// Note that the range may include `usize::MAX` for some rows; this
    /// indicates that the selection extends to the end of that row.
    /// Since this struct has no knowledge of line length, it cannot be
    /// more precise than that.
    /// Must be called on a normalized range!
    pub fn cols_for_row(&self, row: StableRowIndex, rectangular: bool) -> Range<usize> {
        let norm = self.normalize();

        if rectangular {
            if row < norm.start.y || row > norm.end.y {
                0..0
            } else {
                if norm.start.x <= norm.end.x {
                    norm.start.x.range(norm.end.x.saturating_add(1))
                } else {
                    norm.end.x.range(norm.start.x.saturating_add(1))
                }
            }
        } else {
            if row < norm.start.y || row > norm.end.y {
                0..0
            } else if norm.start.y == norm.end.y {
                // A single line selection
                if norm.start.x <= norm.end.x {
                    norm.start.x.range(norm.end.x.saturating_add(1))
                } else {
                    norm.end.x.range(norm.start.x.saturating_add(1))
                }
            } else if row == norm.end.y {
                // last line of multi-line
                SelectionX::Cell(0).range(norm.end.x.saturating_add(1))
            } else if row == norm.start.y {
                // first line of multi-line
                norm.start.x.range(SelectionX::Cell(usize::MAX))
            } else {
                // some "middle" line of multi-line
                0..usize::MAX
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    #[allow(clippy::too_many_lines)]
    fn presented_hyperlinks_survive_local_pane_contention_and_reject_unpresented_frames() {
        let mut terminal = wezterm_term::Terminal::new(
            wezterm_term::TerminalSize {
                rows: 4,
                cols: 80,
                dpi: 96,
                pixel_width: 640,
                pixel_height: 64,
            },
            Arc::new(NativeAnchorTestConfig),
            "presented-links-test",
            "test",
            Box::new(Vec::<u8>::new()),
        );
        terminal.advance_bytes(b"\x1b]8;;https://explicit.example/\x07explicit\x1b]8;;\x07 https://implicit.example/ plain");
        terminal.advance_bytes("\r\n\x1b]8;;https://wide.example/\x07界🙂\x1b]8;;\x07 plain\r\n界 https://implicit.example/ plain".as_bytes());
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize::default())
            .unwrap();
        let writer = pair.master.take_writer().unwrap();
        let child = pair
            .slave
            .spawn_command(portable_pty::CommandBuilder::new("/bin/cat"))
            .unwrap();
        let pane: Arc<dyn Pane> = Arc::new(mux::localpane::LocalPane::new(
            998_402,
            terminal,
            child,
            pair.master,
            writer,
            998_402,
            [0x36; 16],
            "presented links test".to_owned(),
        ));
        struct ChildGuard(Arc<dyn Pane>);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                self.0.kill();
            }
        }
        let _child = ChildGuard(Arc::clone(&pane));
        let rules = vec![termwiz::hyperlink::Rule::new(r"https://[a-z.]+/", "$0").unwrap()];
        let native = pane
            .downcast_ref::<mux::localpane::LocalPane>()
            .unwrap()
            .try_capture_render_frame(None, 0, 0, &rules, false)
            .unwrap();
        let frame = SelectionFrameStamp {
            authority: SelectionAuthority::from_native_frame(&*pane, &native).unwrap(),
            source_sequence: native.source_sequence,
            viewport: native.first,
            geometry: [0; 12],
        };
        let collect = || {
            let mut links = FrameHyperlinks::new(&pane);
            for (index, line) in native.lines.iter().enumerate() {
                let mut spans = Vec::new();
                // The fixture uses a unit-width cell grid; production obtains
                // these intervals from the actual shaped glyph advances.
                for cell in line.visible_cells() {
                    retain_hyperlink_span(
                        &mut spans,
                        cell.attrs().hyperlink(),
                        cell.cell_index() as f32..(cell.cell_index() + cell.width()) as f32,
                        0.0..native.dimensions.cols as f32,
                    );
                }
                links.retain_line(
                    native.first + isize::try_from(index).unwrap(),
                    spans,
                    false,
                    false,
                );
            }
            links
        };
        let mut frames = SelectionFrameState::default();
        frames.begin_attempt();
        frames.stage(Some(frame), Some(frame), true);
        frames.stage_hyperlinks(collect());
        assert!(
            frames
                .hyperlink_at(&pane, frame, native.first, 0.0)
                .is_none(),
            "staging is not presentation"
        );
        assert_eq!(frames.presented(), Some(frame));
        assert_eq!(
            frames
                .hyperlink_at(&pane, frame, native.first, 0.0)
                .unwrap()
                .uri(),
            "https://explicit.example/"
        );
        assert_eq!(
            frames
                .hyperlink_at(&pane, frame, native.first, 9.0)
                .unwrap()
                .uri(),
            "https://implicit.example/"
        );
        assert!(
            frames
                .hyperlink_at(&pane, frame, native.first, 35.0)
                .is_none(),
            "plain text must not inherit a neighboring link"
        );
        assert!(
            native.lines[1].get_cell(1).is_none(),
            "old exact-cell lookup misses the wide continuation"
        );
        for pixel in [0.25, 1.25, 2.25, 3.25] {
            assert_eq!(
                frames
                    .hyperlink_at(&pane, frame, native.first + 1, pixel)
                    .unwrap()
                    .uri(),
                "https://wide.example/",
                "both halves of CJK and emoji glyphs belong to their OSC8 link"
            );
        }
        assert!(
            frames
                .hyperlink_at(&pane, frame, native.first + 1, 4.0)
                .is_none()
        );
        assert!(
            frames
                .hyperlink_at(&pane, frame, native.first + 2, 1.0)
                .is_none(),
            "plain wide glyph must not inherit the following implicit link"
        );
        assert_eq!(
            frames
                .hyperlink_at(&pane, frame, native.first + 2, 3.0)
                .unwrap()
                .uri(),
            "https://implicit.example/"
        );

        // Retained glyph intervals are visual window pixels, not logical
        // columns. An RTL run can be emitted right-to-left, and a ligature's
        // advance covers multiple cells. Cache reuse must preserve the same
        // clipped intervals without re-reading or reshaping the terminal.
        let explicit = frames
            .hyperlink_at(&pane, frame, native.first, 0.0)
            .unwrap();
        let implicit = frames
            .hyperlink_at(&pane, frame, native.first, 9.0)
            .unwrap();
        let mut coalesced = Vec::new();
        for occupied in [10.0..20.0, 20.0..30.0, 5.0..12.0] {
            retain_hyperlink_span(&mut coalesced, Some(&explicit), occupied, 0.0..100.0);
        }
        assert_eq!(coalesced.len(), 1, "LTR adjacency and RTL overlap coalesce");
        assert_eq!((coalesced[0].left, coalesced[0].right), (5.0, 30.0));
        retain_hyperlink_span(&mut coalesced, Some(&explicit), 35.0..40.0, 0.0..100.0);
        retain_hyperlink_span(&mut coalesced, Some(&implicit), 40.0..45.0, 0.0..100.0);
        assert_eq!(
            coalesced.len(),
            3,
            "gaps and different targets must stay separate"
        );
        let mut visual_spans = Vec::new();
        retain_hyperlink_span(
            &mut visual_spans,
            Some(&explicit),
            110.0..130.0,
            100.0..125.0,
        );
        retain_hyperlink_span(
            &mut visual_spans,
            Some(&implicit),
            90.0..110.0,
            100.0..125.0,
        );
        retain_hyperlink_span(
            &mut visual_spans,
            Some(&implicit),
            115.0..115.0,
            100.0..125.0,
        );
        let cached = crate::termwindow::render::LineQuadCacheValue {
            expires: None,
            layers: crate::quad::HeapQuadAllocator::default(),
            current_highlight: None,
            invalidate_on_hover_change: true,
            hyperlinks: visual_spans.clone(),
        };
        let mut quads = crate::quad::HeapQuadAllocator::default();
        let mut fresh = FrameHyperlinks::new(&pane);
        fresh.retain_line(native.first, visual_spans, false, false);
        let mut reused = FrameHyperlinks::new(&pane);
        reused.retain_line(
            native.first,
            cached
                .apply_to_with_hyperlinks(&mut crate::quad::TripleLayerQuadAllocator::Heap(
                    &mut quads,
                ))
                .unwrap(),
            false,
            false,
        );
        for (pixel, expected) in [
            (99.0, None),
            (100.0, Some("https://implicit.example/")),
            (109.5, Some("https://implicit.example/")),
            (110.0, Some("https://explicit.example/")),
            (119.5, Some("https://explicit.example/")),
            (124.5, Some("https://explicit.example/")),
            (125.0, None),
        ] {
            for map in [&fresh, &reused] {
                assert_eq!(
                    map.link_at(&pane, native.first, pixel)
                        .as_ref()
                        .map(|link| link.uri()),
                    expected
                );
            }
        }

        let mut double_height = FrameHyperlinks::new(&pane);
        double_height.retain_line(native.first, cached.hyperlinks.clone(), true, false);
        double_height.retain_line(native.first + 1, Vec::new(), false, true);
        assert_eq!(
            double_height
                .link_at(&pane, native.first + 1, 115.0)
                .unwrap()
                .uri(),
            "https://explicit.example/"
        );
        double_height.retain_line(native.first + 2, Vec::new(), false, true);
        assert!(
            double_height
                .link_at(&pane, native.first + 2, 115.0)
                .is_none(),
            "a bottom row without its preceding displayed top has no hit targets"
        );

        // A submitted but failed replacement cannot change the hit targets.
        let replacement = SelectionFrameStamp {
            source_sequence: frame.source_sequence + 1,
            ..frame
        };
        frames.begin_attempt();
        frames.stage(Some(replacement), Some(replacement), true);
        frames.stage_hyperlinks(FrameHyperlinks::new(&pane));
        assert!(
            frames
                .hyperlink_at(&pane, replacement, native.first, 0.0)
                .is_none()
        );
        assert!(
            frames
                .hyperlink_at(&pane, frame, native.first, 0.0)
                .is_some()
        );
        let mut changed_geometry = frame.geometry;
        changed_geometry[0] = 1;
        assert!(frames.displayed_for_geometry(changed_geometry).is_none());
        let changed_layout = SelectionFrameStamp {
            authority: SelectionAuthority {
                sequence: frame.authority.sequence + 1,
                ..frame.authority
            },
            ..frame
        };
        assert!(frames.for_mouse(Some(changed_layout)).is_none());
        assert!(
            frames
                .hyperlink_at(&pane, changed_layout, native.first, 0.0)
                .is_none()
        );
        // Pointer identity is checked in addition to the coordinate stamp.
        let mut wrong_owner = collect();
        wrong_owner.pane = std::sync::Weak::<mux::localpane::LocalPane>::new();
        assert!(wrong_owner.link_at(&pane, native.first, 0.0).is_none());

        struct HoldTerminal {
            entered: SyncSender<()>,
            release: Receiver<()>,
        }
        impl mux::pane::WithPaneLines for HoldTerminal {
            fn with_lines_mut(&mut self, _: StableRowIndex, lines: &mut [&mut wezterm_term::Line]) {
                assert!(!lines.is_empty());
                self.entered.send(()).unwrap();
                self.release
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap();
            }
        }
        let (entered, acquired) = sync_channel(1);
        let (release, wait_release) = sync_channel(1);
        let held_pane = Arc::clone(&pane);
        let first = native.first;
        let holder = std::thread::spawn(move || {
            held_pane.with_lines_mut(
                first..first + 1,
                &mut HoldTerminal {
                    entered,
                    release: wait_release,
                },
            );
        });
        acquired
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let (completed, result) = sync_channel(1);
        let query_pane = Arc::clone(&pane);
        let query = std::thread::spawn(move || {
            // This is a real LocalPane terminal acquisition barrier, not a
            // synthetic busy flag. The old hit-test path loses the link here.
            assert!(SelectionAuthority::capture_source(&*query_pane).is_none());
            struct OldLookup(Option<Arc<termwiz::hyperlink::Hyperlink>>);
            impl mux::pane::WithPaneLines for OldLookup {
                fn with_lines_mut(
                    &mut self,
                    _: StableRowIndex,
                    lines: &mut [&mut wezterm_term::Line],
                ) {
                    self.0 = lines
                        .first()
                        .and_then(|line| line.get_cell(0))
                        .and_then(|cell| cell.attrs().hyperlink().cloned());
                }
            }
            let mut old = OldLookup(None);
            query_pane.with_lines_mut_and_apply_hyperlinks(first..first + 1, &rules, &mut old);
            assert!(
                old.0.is_none(),
                "negative control must exercise the busy source"
            );
            let displayed = frames.displayed_for_geometry(frame.geometry).unwrap();
            let link = frames
                .hyperlink_at(&query_pane, displayed, first, 0.0)
                .unwrap();
            completed.send(link.uri().to_owned()).unwrap();
            frames
        });
        let observed = result.recv_timeout(std::time::Duration::from_secs(2));
        // Always release and reap both threads before asserting the deadline.
        release.send(()).unwrap();
        holder.join().unwrap();
        let mut frames = query.join().unwrap();
        assert_eq!(observed.unwrap(), "https://explicit.example/");
        assert_eq!(
            frames.for_mouse(Some(frame)),
            Some(frame),
            "hit testing must not alter selection authority"
        );

        // A successful no-link replacement clears the old target, and an
        // incomplete submitted pane cannot retain a previous frame's links.
        assert_eq!(frames.presented(), Some(replacement));
        assert!(
            frames
                .hyperlink_at(&pane, replacement, first, 0.0)
                .is_none()
        );
        frames.begin_attempt();
        frames.stage(Some(frame), Some(frame), false);
        frames.stage_hyperlinks(collect());
        assert_eq!(frames.presented(), None);
        assert!(frames.hyperlink_at(&pane, frame, first, 0.0).is_none());

        // A real registry binding changes authority even for the same pane
        // allocation. No process-global mux or config is installed by this test.
        let unregistered = collect();
        let owner = Arc::new(mux::Mux::new(None));
        owner.add_pane(&pane).unwrap();
        assert!(unregistered.link_at(&pane, first, 0.0).is_none());
        let registered = collect();
        assert!(registered.link_at(&pane, first, 0.0).is_some());
        let registration = owner.capture_pane_registration(&pane).unwrap();
        assert!(registration.retire_if_current());
        assert!(registered.link_at(&pane, first, 0.0).is_none());
    }

    #[derive(Debug)]
    struct NativeAnchorTestConfig;

    impl wezterm_term::TerminalConfiguration for NativeAnchorTestConfig {
        fn color_palette(&self) -> wezterm_term::color::ColorPalette {
            Default::default()
        }
    }

    fn native_anchor_fixture() -> (wezterm_term::Terminal, Selection, String) {
        let size = wezterm_term::TerminalSize {
            rows: 24,
            cols: 80,
            dpi: 96,
            pixel_width: 640,
            pixel_height: 384,
        };
        let mut term = wezterm_term::Terminal::new(
            size,
            std::sync::Arc::new(NativeAnchorTestConfig),
            "selection-test",
            "test",
            Box::new(Vec::<u8>::new()),
        );
        let text = "café e\u{301} 中文 🙂 end café e\u{301} 中文 🙂 end café e\u{301} 中文 🙂 end TAIL\nsecond line Ω";
        term.advance_bytes(
            format!(
                "FT_HELD_READY\r\nanchor row\r\n{}",
                text.replace('\n', "\r\n")
            )
            .as_bytes(),
        );
        let mut selection = Selection::default();
        selection.begin(SelectionCoordinate::x_y(0, 2));
        selection.range = Some(
            SelectionRange::start(SelectionCoordinate::x_y(0, 2))
                .extend(SelectionCoordinate::x_y(12, 3)),
        );
        selection.seqno = term.current_seqno();
        selection.authority = Some(SelectionAuthority {
            source: 1,
            sequence: selection.seqno,
            geometry: (80, 24, 96, 640, 384),
            alternate: false,
        });
        let token = term
            .screen_mut()
            .capture_selection_anchor(selection.seqno, selection.native_points())
            .unwrap();
        selection.remember_native_anchor(token);
        (term, selection, text.to_string())
    }

    fn live_native_selection_text(term: &wezterm_term::Terminal, selection: &Selection) -> String {
        let lines: Vec<_> = term
            .screen()
            .lines_in_phys_range(0..term.screen().scrollback_rows())
            .into_iter()
            .enumerate()
            .map(|(row, line)| mux::pane::LogicalLine {
                first_row: term.screen().phys_to_stable_row_index(row),
                logical: line.clone(),
                physical_lines: vec![line],
            })
            .collect();
        crate::termwindow::selected_text_from_logical_lines(&lines, selection.range.unwrap(), false)
    }

    #[test]
    fn native_selection_rebase_rejects_obsolete_frame_damage_but_not_later_edits() {
        let (mut term, mut selection, expected) = native_anchor_fixture();
        let before_resize = selection.seqno;
        let token = selection.native_anchor().unwrap().clone();
        term.resize(wezterm_term::TerminalSize {
            rows: 24,
            cols: 37,
            dpi: 96,
            pixel_width: 296,
            pixel_height: 384,
        });
        let frame_sequence = term.current_seqno();
        assert!(frame_sequence > before_resize);
        let damage = term.screen().get_changed_stable_rows(0..24, before_resize);
        assert!(
            !damage.is_empty(),
            "the captured frame must carry real reflow damage"
        );
        let mut authority = selection.authority.unwrap();
        authority.sequence = frame_sequence;
        authority.geometry = (37, 24, 96, 296, 384);

        // The terminal lock is released after capturing a frame. Output outside
        // the selected rows can arrive before the GUI resolves its native token.
        term.advance_bytes(b"\x1b[20;1HUNRELATED OUTPUT");
        let resolved_sequence = term.current_seqno();
        assert!(resolved_sequence > frame_sequence);
        let points = term
            .screen()
            .resolve_selection_anchor(&token, resolved_sequence)
            .unwrap();
        assert!(selection.rebase_native_anchor(points, authority, resolved_sequence));
        assert_eq!(live_native_selection_text(&term, &selection), expected);
        assert!(
            damage
                .iter()
                .any(|row| selection.range.unwrap().rows().contains(row))
        );
        assert!(crate::termwindow::native_selection_covers_frame(
            selection.seqno,
            frame_sequence,
            before_resize,
        ));

        // A real edit after the validated rebase invalidates the same token and
        // must still clear the selection when that newer frame is processed.
        let edited_row = selection.range.unwrap().normalize().start.y;
        term.advance_bytes(format!("\x1b[{};1HCHANGED", edited_row + 1).as_bytes());
        let edited_sequence = term.current_seqno();
        assert!(edited_sequence > resolved_sequence);
        assert!(
            term.screen()
                .resolve_selection_anchor(&token, edited_sequence)
                .is_none()
        );
        assert!(
            term.screen()
                .get_changed_stable_rows(0..24, resolved_sequence)
                .contains(&edited_row)
        );
        assert!(!crate::termwindow::native_selection_covers_frame(
            selection.seqno,
            edited_sequence,
            selection.seqno,
        ));
        assert!(
            !crate::termwindow::native_selection_covers_frame(
                selection.seqno,
                frame_sequence,
                selection.seqno,
            ),
            "an unexplained source regression must not be treated as a successful rebase"
        );
        for (selected, frame) in [
            (termwiz::surface::SequenceNo::MAX, frame_sequence),
            (selection.seqno, termwiz::surface::SequenceNo::MAX),
        ] {
            assert!(!crate::termwindow::native_selection_covers_frame(
                selected,
                frame,
                before_resize
            ));
        }
    }

    #[test]
    fn native_selection_commit_retains_released_intent_through_capture_contention() {
        let (mut term, mut committed, _) = native_anchor_fixture();
        let previous = committed.clone();
        let mut desired = committed.clone();
        desired.range.as_mut().unwrap().end = SelectionCoordinate::x_y(5, 3);
        let mut pending = PendingNativeSelection::new(desired);
        // The LocalPane producer test holds the real terminal lock and checks
        // this typed result. Exercise its actual GUI mapping and transaction.
        let busy = || Err(mux::localpane::SelectionAnchorCaptureError::Busy).into();
        {
            assert_eq!(pending.try_commit(&mut committed, busy()), None);
            // Mouse release binds the copy to this endpoint, not `previous`.
            pending.copy = Some(config::keyassignment::ClipboardCopyDestination::Clipboard);
            for _ in 0..3 {
                assert!(pending.take_paint_retry());
                assert_eq!(pending.try_commit(&mut committed, busy()), None);
                assert_eq!(committed, previous);
            }
            assert!(!pending.take_paint_retry());
            assert!(pending.copy.is_some());
        }
        let captured = term
            .screen_mut()
            .capture_selection_anchor(pending.desired.seqno, pending.desired.native_points());
        assert!(captured.is_some());
        assert_eq!(
            pending.try_commit(&mut committed, Ok(captured).into()),
            Some(NativeSelectionCommit::Applied {
                needs_repaint: true
            })
        );
        assert_eq!(committed.range, pending.desired.range);
        assert!(committed.native_anchor().is_some());
        let captured_again = term
            .screen_mut()
            .capture_selection_anchor(pending.desired.seqno, pending.desired.native_points());
        assert_eq!(
            pending.try_commit(&mut committed, Ok(captured_again).into()),
            Some(NativeSelectionCommit::Applied {
                needs_repaint: false
            }),
            "copy retries with unchanged visible selection must not cause a paint loop"
        );
        let expected = live_native_selection_text(&term, &committed);
        assert_ne!(expected, live_native_selection_text(&term, &previous));
        let token = committed.native_anchor().unwrap().clone();
        term.resize(wezterm_term::TerminalSize {
            rows: 24,
            cols: 37,
            dpi: 96,
            pixel_width: 296,
            pixel_height: 384,
        });
        let sequence = term.current_seqno();
        let points = term
            .screen()
            .resolve_selection_anchor(&token, sequence)
            .unwrap();
        let mut authority = committed.authority.unwrap();
        authority.sequence = sequence;
        authority.geometry = (37, 24, 96, 296, 384);
        assert!(committed.rebase_native_anchor(points, authority, sequence));
        assert_eq!(live_native_selection_text(&term, &committed), expected);
    }

    #[test]
    fn native_selection_commit_rejects_obsolete_intent_without_replacing_anchor() {
        let (_, mut committed, _) = native_anchor_fixture();
        let previous = committed.clone();
        let mut desired = committed.clone();
        desired.range.as_mut().unwrap().end = SelectionCoordinate::x_y(1, 2);
        let pending = PendingNativeSelection::new(desired);
        assert_eq!(
            pending.try_commit(
                &mut committed,
                Err(mux::localpane::SelectionAnchorCaptureError::SourceChanged).into()
            ),
            Some(NativeSelectionCommit::Invalidated)
        );
        assert_eq!(committed, previous);
        assert!(committed.native_anchor().is_some());
    }

    #[test]
    fn native_selection_commit_preserves_current_unremappable_selection() {
        let (_, mut committed, _) = native_anchor_fixture();
        let mut desired = committed.clone();
        desired.range.as_mut().unwrap().end = SelectionCoordinate::x_y(1, 2);
        let pending = PendingNativeSelection::new(desired);
        assert_eq!(
            pending.try_commit(&mut committed, Ok(None).into()),
            Some(NativeSelectionCommit::Applied {
                needs_repaint: true
            })
        );
        assert_eq!(committed.range, pending.desired.range);
        assert!(committed.native_anchor().is_none());
        assert!(committed.is_authorized_by(pending.desired.authority));
        let mut resized = pending.desired.authority.unwrap();
        resized.geometry.0 += 1;
        assert!(committed.is_invalidated_by(Some(resized)));
    }

    #[test]
    fn native_selection_transports_real_parser_endpoints_and_reads_live_unicode_text() {
        let (mut term, mut selection, expected) = native_anchor_fixture();
        assert_eq!(live_native_selection_text(&term, &selection), expected);
        let token = selection.native_anchor().unwrap().clone();
        for cols in [40, 36, 44, 80] {
            let mut size = term.get_size();
            size.cols = cols;
            size.pixel_width = cols * 8;
            term.resize(size);
            let sequence = term.current_seqno();
            let points = term
                .screen()
                .resolve_selection_anchor(&token, sequence)
                .unwrap();
            let authority = SelectionAuthority {
                sequence,
                geometry: (cols, 24, 96, cols * 8, 384),
                ..selection.authority.unwrap()
            };
            assert!(
                selection.is_invalidated_by(Some(authority)),
                "unmapped physical points must remain unauthorized"
            );
            assert!(selection.rebase_native_anchor(points, authority, sequence));
            assert!(selection.is_authorized_by(Some(authority)));
            assert_eq!(live_native_selection_text(&term, &selection), expected);
            if cols == 40 {
                assert_eq!(
                    selection.range.unwrap().end,
                    SelectionCoordinate::x_y(12, 4)
                );
                assert_eq!(term.cursor_pos().x, 13);
                assert_eq!(term.cursor_pos().y, 4);
            }
        }
        // Reads still use the current cells: changing selected content cannot
        // be hidden by a frozen text facade, nor authorize a later transport.
        term.advance_bytes(b"\x1b[3;1HZ");
        assert!(live_native_selection_text(&term, &selection).starts_with("Zafé"));
        let mut size = term.get_size();
        size.cols = 40;
        term.resize(size);
        assert!(
            term.screen()
                .resolve_selection_anchor(&token, term.current_seqno())
                .is_none()
        );
    }

    #[test]
    fn native_selection_transport_rejects_replaced_gesture_and_cleared_selection() {
        let (mut term, mut selection, _) = native_anchor_fixture();
        let token = selection.native_anchor().unwrap().clone();
        let mut size = term.get_size();
        size.cols = 40;
        term.resize(size);
        let sequence = term.current_seqno();
        let points = term
            .screen()
            .resolve_selection_anchor(&token, sequence)
            .unwrap();
        let authority = SelectionAuthority {
            sequence,
            geometry: (40, 24, 96, 640, 384),
            ..selection.authority.unwrap()
        };
        selection.range.as_mut().unwrap().end.x = SelectionX::Cell(5);
        let new_range = selection.range;
        assert!(!selection.rebase_native_anchor(points, authority, sequence));
        assert_eq!(selection.range, new_range);
        selection.clear();
        assert!(!selection.rebase_native_anchor(points, authority, sequence));
        assert!(selection.origin.is_none() && selection.range.is_none());
    }

    #[test]
    fn native_selection_before_zero_preserves_text_for_both_endpoint_roles_and_directions() {
        for starts_before in [false, true] {
            for reverse in [false, true] {
                let (mut term, mut selection, _) = native_anchor_fixture();
                let mut size = term.get_size();
                size.cols = 40;
                term.resize(size);
                let before = SelectionCoordinate {
                    x: SelectionX::BeforeZero,
                    y: 3,
                };
                let (start, end) = if starts_before {
                    (before, SelectionCoordinate::x_y(12, 4))
                } else {
                    (SelectionCoordinate::x_y(0, 2), before)
                };
                selection.origin = Some(SelectionCoordinate::x_y(0, 2));
                selection.range = Some(if reverse {
                    SelectionRange {
                        start: end,
                        end: start,
                    }
                } else {
                    SelectionRange { start, end }
                });
                selection.seqno = term.current_seqno();
                selection.authority = Some(SelectionAuthority {
                    sequence: selection.seqno,
                    geometry: (40, 24, 96, 640, 384),
                    ..selection.authority.unwrap()
                });
                let token = term
                    .screen_mut()
                    .capture_selection_anchor(selection.seqno, selection.native_points())
                    .unwrap();
                selection.remember_native_anchor(token.clone());
                let expected = live_native_selection_text(&term, &selection);
                assert!(!expected.is_empty());
                if !starts_before {
                    let row = term.screen().lines_in_phys_range(2..3).remove(0);
                    assert_eq!(row.len(), 38);
                    assert_eq!(
                        row.as_str(),
                        "café e\u{301} 中文 🙂 end café e\u{301} 中文 🙂 end "
                    );
                    assert!(row.last_cell_was_wrapped());
                    // Endpoint extraction trims trailing blanks regardless of
                    // whether the endpoint lies at a physical soft wrap.
                    assert_eq!(
                        expected,
                        "café e\u{301} 中文 🙂 end café e\u{301} 中文 🙂 end"
                    );
                }
                size.cols = 80;
                term.resize(size);
                let sequence = term.current_seqno();
                let points = term
                    .screen()
                    .resolve_selection_anchor(&token, sequence)
                    .unwrap();
                let authority = SelectionAuthority {
                    sequence,
                    geometry: (80, 24, 96, 640, 384),
                    ..selection.authority.unwrap()
                };
                assert!(selection.rebase_native_anchor(points, authority, sequence));
                if !starts_before {
                    let endpoint = if reverse {
                        selection.range.unwrap().start
                    } else {
                        selection.range.unwrap().end
                    };
                    assert_eq!(endpoint, SelectionCoordinate::x_y(37, 2));
                }
                assert_eq!(
                    live_native_selection_text(&term, &selection),
                    expected,
                    "starts_before={starts_before}, reverse={reverse}"
                );
            }
        }
    }

    #[test]
    fn selection_coordinates_cannot_be_reauthorized_by_drag_damage_sequence() {
        let authority = SelectionAuthority {
            source: 1,
            sequence: 10,
            geometry: (80, 24, 96, 800, 480),
            alternate: false,
        };
        let mut selection = Selection::default();
        selection.begin(SelectionCoordinate::x_y(0, -100));
        selection.range = Some(SelectionRange::start(SelectionCoordinate::x_y(0, -100)));
        selection.authority = Some(authority);
        selection.seqno = 10;
        assert!(selection.is_authorized_by(Some(authority)));
        assert!(!selection.is_authorized_by(None));
        assert!(!selection.is_invalidated_by(None));
        assert!(!selection.is_invalidated_by(Some(authority)));
        let changed = SelectionAuthority {
            sequence: 11,
            ..authority
        };
        selection.seqno = 100; // dragging/dirty updates do not move the anchor epoch
        assert!(!selection.is_authorized_by(Some(changed)));
        assert!(selection.is_invalidated_by(Some(changed)));
        assert!(!selection.is_authorized_by(Some(SelectionAuthority {
            source: 2,
            ..authority
        })));
        assert!(!selection.is_authorized_by(Some(SelectionAuthority {
            alternate: true,
            ..authority
        })));
        assert!(!selection.is_authorized_by(Some(SelectionAuthority {
            geometry: (79, 24, 96, 790, 480),
            ..authority
        })));
        selection.clear();
        assert!(!selection.is_authorized_by(Some(authority)));
        assert!(selection.origin.is_none() && selection.range.is_none());
    }

    #[test]
    fn selection_frame_authority_requires_complete_successful_presentation() {
        let a = SelectionFrameStamp {
            authority: SelectionAuthority {
                source: 1,
                sequence: 10,
                geometry: (80, 24, 96, 800, 480),
                alternate: false,
            },
            source_sequence: 10,
            viewport: -100,
            geometry: [0; 12],
        };
        let mut b = a;
        b.authority.sequence = 11;
        b.source_sequence = 11;
        let mut state = SelectionFrameState::default();
        state.begin_attempt();
        state.stage(Some(a), Some(a), true);
        assert_eq!(state.for_mouse(Some(a)), None); // geometry is not presentation
        assert_eq!(state.presented(), Some(a));
        assert_eq!(state.for_mouse(Some(a)), Some(a));
        state.begin_attempt();
        state.stage(Some(b), Some(b), true);
        // Failed swap leaves A displayed; damage/query B cannot authorize B.
        assert_eq!(state.for_mouse(Some(b)), None);
        assert_eq!(state.for_mouse(Some(a)), Some(a));
        state.begin_attempt(); // atlas retry must discard the B candidate
        state.stage(Some(b), Some(b), false); // incomplete cold rows
        assert_eq!(state.presented(), None);
        assert_eq!(state.for_mouse(Some(b)), None);
        state.begin_attempt();
        state.stage(Some(a), Some(b), true); // layout changed during snapshot
        assert_eq!(state.presented(), None);
        assert_eq!(state.for_mouse(Some(b)), None);
        state.begin_attempt();
        state.stage(Some(b), Some(b), true);
        assert_eq!(state.presented(), Some(b));
        assert_eq!(state.for_mouse(Some(b)), Some(b));
        let mut scrolled = b;
        scrolled.viewport += 1;
        assert_eq!(state.for_mouse(Some(scrolled)), None);
        let mut rescaled = b;
        rescaled.geometry[0] += 1;
        assert_eq!(state.for_mouse(Some(rescaled)), None);
        let mut output = b;
        output.source_sequence += 1;
        assert_eq!(state.for_mouse(Some(output)), Some(b));
        state.begin_attempt();
        state.stage(Some(b), Some(output), true);
        assert_eq!(state.presented(), Some(b));
        assert_eq!(state.for_mouse(Some(output)), Some(b));
        state.begin_attempt(); // pane omitted by successful next frame
        assert_eq!(state.presented(), None);
        assert_eq!(state.for_mouse(Some(b)), None);
    }

    #[test]
    fn selection_drag_survives_unavailable_frame_and_autoscroll_until_layout_changes() {
        let frame = SelectionFrameStamp {
            authority: SelectionAuthority {
                source: 1,
                sequence: 10,
                geometry: (80, 24, 96, 800, 480),
                alternate: false,
            },
            source_sequence: 10,
            viewport: 100,
            geometry: [0; 12],
        };
        let mut selection = Selection::default();
        let anchor = SelectionCoordinate::x_y(3, 110);
        selection.begin(anchor);
        selection.authority = Some(frame.authority);
        let mut state = SelectionFrameState::default();
        state.stage(Some(frame), Some(frame), true);
        assert_eq!(state.presented(), Some(frame));

        // Unlike an established drag, a busy press must retain its ORIGINAL
        // click until a usable frame is available, regardless of later motion.
        let pending = PendingSelectionStart {
            frame,
            coordinate: anchor,
            mode: SelectionMode::Cell,
            button: window::MousePress::Left,
            paint_retries_remaining: 3,
            released: false,
            end: None,
            copy: None,
            read: None,
            deadline: std::time::Instant::now() + WordLineSelectionRead::FIXED_DEADLINE,
            deadline_wake: None,
        };
        let mut retries = pending.clone();
        for _ in 0..3 {
            assert!(retries.take_paint_retry());
        }
        assert!(!retries.take_paint_retry());
        assert!(!pending.is_expired());
        assert!(matches!(
            pending.resolve(None, &state),
            PendingSelectionResolution::Wait
        ));
        let mut unpresented = SelectionFrameState::default();
        assert!(matches!(
            pending.resolve(Some(frame), &unpresented),
            PendingSelectionResolution::Wait
        ));
        unpresented.stage(Some(frame), Some(frame), true);
        assert_eq!(unpresented.presented(), Some(frame));
        assert!(matches!(
            pending.resolve(Some(frame), &unpresented),
            PendingSelectionResolution::Ready
        ));
        assert_eq!(pending.coordinate, anchor);
        assert_eq!(pending.mode, SelectionMode::Cell);
        // No motion event arrived while the source was busy. The release
        // position itself must become the final endpoint, even if release was
        // marked before coordinate conversion in the native event handler.
        let mut released = pending.clone();
        released.released = true;
        released.retain_endpoint(
            frame,
            wezterm_term::input::ClickPosition {
                column: 9,
                row: 2,
                x_pixel_offset: 0,
                y_pixel_offset: 0,
            },
            anchor.y + 2,
            Some(window::MousePress::Left),
        );
        assert_eq!(released.end.unwrap().0.column, 9);
        assert_eq!(released.end.unwrap().1, anchor.y + 2);
        assert_eq!(released.coordinate, anchor);
        released.retain_endpoint(
            frame,
            wezterm_term::input::ClickPosition {
                column: 18,
                row: 4,
                x_pixel_offset: 0,
                y_pixel_offset: 0,
            },
            anchor.y + 4,
            None,
        );
        assert_eq!(
            released.end.unwrap().0.column,
            9,
            "later motion cannot alter a released gesture"
        );
        assert!(matches!(
            released.resolve(None, &state),
            PendingSelectionResolution::Wait
        ));
        assert!(matches!(
            released.resolve(Some(frame), &state),
            PendingSelectionResolution::Ready
        ));
        let mut different_geometry = frame.geometry;
        different_geometry[0] += 1;
        assert!(state.displayed_for_geometry(different_geometry).is_none());

        // The parser owns the lock: neither extend nor destroy the anchor.
        assert_eq!(state.for_mouse(None), None);
        assert!(!selection.is_invalidated_by(None));
        assert_eq!(selection.origin, Some(anchor));
        let scrolled = SelectionFrameStamp {
            viewport: 101,
            source_sequence: 12,
            ..frame
        };
        assert!(matches!(
            pending.resolve(Some(scrolled), &state),
            PendingSelectionResolution::Invalidated
        ));
        assert_eq!(state.for_mouse(Some(scrolled)), None);
        assert!(!selection.is_invalidated_by(Some(scrolled.authority)));
        state.begin_attempt();
        state.stage(Some(scrolled), Some(scrolled), true);
        assert_eq!(state.presented(), Some(scrolled));
        assert_eq!(state.for_mouse(Some(scrolled)), Some(scrolled));
        assert_eq!(selection.origin, Some(anchor));

        let replaced = SelectionAuthority {
            sequence: 13,
            ..frame.authority
        };
        assert!(matches!(
            pending.resolve(
                Some(SelectionFrameStamp {
                    authority: replaced,
                    ..frame
                }),
                &state
            ),
            PendingSelectionResolution::Invalidated
        ));
        assert!(selection.is_invalidated_by(Some(replaced)));
        assert_eq!(
            state.for_mouse(Some(SelectionFrameStamp {
                authority: replaced,
                ..scrolled
            })),
            None
        );
    }

    #[test]
    fn smart_match_logical_x_range_selects_url_inside_quotes() {
        let text = "open \"https://example.com/foo?bar=1\" now";
        let click_x = text.find("example").unwrap();

        let smart = smart_match_logical_x_range(text, click_x).expect("URL match");

        assert_eq!(&text[smart.range.clone()], "https://example.com/foo?bar=1");
        assert_eq!(smart.kind, SelectionPatternKind::Url);
        assert_eq!(smart.text, "https://example.com/foo?bar=1");
    }

    #[test]
    fn smart_match_logical_x_range_returns_none_on_whitespace() {
        let text = "alpha beta";
        let click_x = text.find(' ').unwrap();

        assert!(smart_match_logical_x_range(text, click_x).is_none());
    }

    #[test]
    fn byte_offset_for_logical_x_handles_wide_prefix() {
        let text = "表 https://example.com";
        let click_x = 3;

        assert_eq!(byte_offset_for_logical_x(text, click_x), "表 ".len());
    }

    #[test]
    fn word_click_range_empty_stays_at_clicked_point() {
        let line: termwiz::surface::line::Line = "abc".into();
        let logical = mux::pane::LogicalLine {
            physical_lines: vec![line.clone()],
            logical: line,
            first_row: 0,
        };
        let start = SelectionCoordinate::x_y(1, 0);

        let range = SelectionRange::from_logical_click_range(start, &logical, 0..0);

        assert_eq!(range, SelectionRange { start, end: start });
    }

    // ----------------------------------------------------------------
    // br-ft-t5j0a: smart_match_in_line wiring proof at the GUI-helper
    // layer. The Pane-driven `smart_or_line_around` integration is
    // tested via the bin-level selection tests; these unit tests
    // pin the substrate the new function consumes so a future
    // refactor of `smart_match_in_line` semantics surfaces here
    // immediately.
    // ----------------------------------------------------------------

    #[test]
    fn smart_match_in_line_picks_url_in_full_line_span() {
        // Triple-click selects the widest pattern fully contained in
        // the line. URL inside text → URL wins.
        let text = "see https://example.com/long-path?param=value for details";
        let m = smart_match_in_line(text, 0, text.len()).expect("URL match");
        assert_eq!(m.kind, SelectionPatternKind::Url);
        assert_eq!(
            &text[m.span_start..m.span_end],
            "https://example.com/long-path?param=value"
        );
    }

    #[test]
    fn smart_match_in_line_returns_none_on_plain_text_only_line() {
        // No smart pattern → fallback to legacy line_around must fire
        // (the Pane-driver test verifies that path; here we just
        // confirm the substrate signals "no match").
        let text = "alpha beta gamma delta";
        assert!(smart_match_in_line(text, 0, text.len()).is_none());
    }

    #[test]
    fn smart_match_in_line_picks_email_when_present() {
        let text = "ping ops@example.com on incident";
        let m = smart_match_in_line(text, 0, text.len()).expect("Email match");
        assert_eq!(m.kind, SelectionPatternKind::Email);
        assert_eq!(&text[m.span_start..m.span_end], "ops@example.com");
    }

    #[test]
    fn smart_match_in_line_skips_partial_pattern_when_constrained() {
        // span constrained to before the URL starts → no match.
        let text = "echo https://example.com/x";
        let cutoff = text.find("https").unwrap();
        assert!(smart_match_in_line(text, 0, cutoff).is_none());
    }

    #[test]
    fn smart_match_in_line_url_inside_quotes_wins_over_shell_quoted() {
        // Same drop_shell_quoted_supersets pre-filter the double-
        // click path uses applies here: URL inside 'quotes' beats
        // the surrounding ShellQuoted span.
        let text = r"echo 'https://example.com/foo'";
        let m = smart_match_in_line(text, 0, text.len()).expect("URL match");
        assert_eq!(m.kind, SelectionPatternKind::Url);
        assert_eq!(&text[m.span_start..m.span_end], "https://example.com/foo");
    }

    #[test]
    fn smart_or_word_around_wrapped_literal_unicode_smart_match() {
        // Wrapped logical line with a URL containing literal Unicode (Japanese chars).
        // Physical row 10: "visit https://examp" has 18 columns.
        // Physical row 11: "le.com/日本語 path" (7 ASCII + 6 CJK wide + 4 ASCII)
        // Combined logical line: "visit https://example.com/日本語 path"
        let p0: termwiz::surface::line::Line = "visit https://examp".into();
        let p1: termwiz::surface::line::Line = "le.com/日本語 path".into();
        let full: termwiz::surface::line::Line = "visit https://example.com/日本語 path".into();
        let logical = mux::pane::LogicalLine {
            physical_lines: vec![p0, p1],
            logical: full,
            first_row: 10,
        };

        // Click on physical row 10, column 8 (within "https://examp")
        let start = SelectionCoordinate::x_y(8, 10);
        let (range, pick) = SelectionRange::smart_or_word_around_in_logical_lines(
            start,
            std::slice::from_ref(&logical),
        );

        let pick = pick.expect("URL match with literal Unicode should succeed");
        assert_eq!(pick.kind, SelectionPatternKind::Url);
        assert_eq!(pick.text, "https://example.com/日本語");

        // "visit " is 6 cols, URL starts at col 6 on row 10
        assert_eq!(range.start, SelectionCoordinate::x_y(6, 10));
        // URL ends at logical x 31 (exclusive). Its last cell is at
        // 30 - 18 = 12 on row 11 (second cell of '語').
        assert_eq!(range.end, SelectionCoordinate::x_y(12, 11));

        // Click directly on row 11 on the wide Unicode character '本'
        // 'le.com/' is 7 cols, '日' is 2 cols (cols 7..9), '本' is cols 9..11
        let start_unicode = SelectionCoordinate::x_y(9, 11);
        let (range_u, pick_u) = SelectionRange::smart_or_word_around_in_logical_lines(
            start_unicode,
            std::slice::from_ref(&logical),
        );
        let pick_u = pick_u.expect("Click on wide Unicode inside URL should match");
        assert_eq!(pick_u.text, "https://example.com/日本語");
        assert_eq!(range_u, range);
    }

    #[test]
    fn smart_or_word_around_wrapped_literal_unicode_fallback() {
        // Plain wrapped line with CJK wide characters and no smart pattern.
        // Verifies that fallback uses the same snapshot and respects word boundary across wrap.
        // Row 20: "prefix 日本" (7 ASCII cols + 4 CJK cols = 11 cols)
        // Row 21: "語word suffix" (2 CJK cols + 4 ASCII cols + 7 ASCII cols = 13 cols)
        // Combined logical line: "prefix 日本語word suffix"
        let p0: termwiz::surface::line::Line = "prefix 日本".into();
        let p1: termwiz::surface::line::Line = "語word suffix".into();
        let full: termwiz::surface::line::Line = "prefix 日本語word suffix".into();
        let logical = mux::pane::LogicalLine {
            physical_lines: vec![p0, p1],
            logical: full,
            first_row: 20,
        };

        // Click on row 21, column 0 ('語')
        let start = SelectionCoordinate::x_y(0, 21);
        let (range, pick) = SelectionRange::smart_or_word_around_in_logical_lines(
            start,
            std::slice::from_ref(&logical),
        );

        // Fallback should fire without smart pick
        assert_eq!(pick, None);

        // Word "日本語word" starts at logical x 7 (col 7 on row 20)
        assert_eq!(range.start, SelectionCoordinate::x_y(7, 20));
        // Word is 6 CJK cols + 4 ASCII cols = 10 cols.
        // In row 20: 4 cols ('日本'). Remaining 6 cols in row 21: '語' (2) + 'word' (4) = 6 cols (cols 0..6, end-1 is col 5).
        assert_eq!(range.end, SelectionCoordinate::x_y(5, 21));

        // Direct word_around_in_logical_lines produces the exact same range
        let direct_range =
            SelectionRange::word_around_in_logical_lines(start, std::slice::from_ref(&logical));
        assert_eq!(direct_range, range);
    }

    #[test]
    fn smart_or_word_around_before_zero_and_boundary_cases() {
        let p0: termwiz::surface::line::Line = "hello world".into();
        let logical = mux::pane::LogicalLine {
            physical_lines: vec![p0],
            logical: "hello world".into(),
            first_row: 5,
        };

        // BeforeZero coordinate: must preserve start == end and return None
        let bz = SelectionCoordinate {
            x: SelectionX::BeforeZero,
            y: 5,
        };
        let (range_bz, pick_bz) = SelectionRange::smart_or_word_around_in_logical_lines(
            bz,
            std::slice::from_ref(&logical),
        );
        assert_eq!(pick_bz, None);
        assert_eq!(range_bz, SelectionRange { start: bz, end: bz });

        // Past-end coordinate (e.g. col 999): must preserve start == end and return None
        let past_end = SelectionCoordinate::x_y(999, 5);
        let (range_pe, pick_pe) = SelectionRange::smart_or_word_around_in_logical_lines(
            past_end,
            std::slice::from_ref(&logical),
        );
        assert_eq!(pick_pe, None);
        assert_eq!(
            range_pe,
            SelectionRange {
                start: past_end,
                end: past_end,
            }
        );

        // Non-matching row coordinate (row 99): must return fallback start == end
        let wrong_row = SelectionCoordinate::x_y(2, 99);
        let (range_wr, pick_wr) = SelectionRange::smart_or_word_around_in_logical_lines(
            wrong_row,
            std::slice::from_ref(&logical),
        );
        assert_eq!(pick_wr, None);
        assert_eq!(
            range_wr,
            SelectionRange {
                start: wrong_row,
                end: wrong_row,
            }
        );
    }

    #[test]
    fn smart_or_line_around_wrapped_smart_match() {
        // Wrapped line containing an email address across the wrap.
        // Row 30: "please write to " has 15 columns.
        // Row 31: "support@example.com for help" (27 cols)
        let p0: termwiz::surface::line::Line = "please write to ".into();
        let p1: termwiz::surface::line::Line = "support@example.com for help".into();
        let full: termwiz::surface::line::Line =
            "please write to support@example.com for help".into();
        let logical = mux::pane::LogicalLine {
            physical_lines: vec![p0, p1],
            logical: full,
            first_row: 30,
        };

        let start = SelectionCoordinate::x_y(0, 30);
        let (range, pick) = SelectionRange::smart_or_line_around_in_logical_lines(
            start,
            std::slice::from_ref(&logical),
        );

        let pick = pick.expect("Email smart pattern in line should be picked");
        assert_eq!(pick.kind, SelectionPatternKind::Email);
        assert_eq!(pick.text, "support@example.com");

        // Email starts at logical col 15 -> on row 31, col 0
        assert_eq!(range.start, SelectionCoordinate::x_y(0, 31));
        // Email length 19 -> on row 31, col 18 (inclusive end)
        assert_eq!(range.end, SelectionCoordinate::x_y(18, 31));
    }

    #[test]
    fn smart_or_line_around_wrapped_plain_fallback() {
        // Plain wrapped line with no smart pattern
        // Row 40: "first physical line of text "
        // Row 41: "second physical line of text"
        let p0: termwiz::surface::line::Line = "first physical line of text ".into();
        let p1: termwiz::surface::line::Line = "second physical line of text".into();
        let full: termwiz::surface::line::Line =
            "first physical line of text second physical line of text".into();
        let logical = mux::pane::LogicalLine {
            physical_lines: vec![p0, p1],
            logical: full,
            first_row: 40,
        };

        // Click within physical row 41
        let start = SelectionCoordinate::x_y(5, 41);
        let (range, pick) = SelectionRange::smart_or_line_around_in_logical_lines(
            start,
            std::slice::from_ref(&logical),
        );

        assert_eq!(pick, None);
        // Line selection spans row 40 col 0 to row 41 col usize::MAX
        assert_eq!(range.start, SelectionCoordinate::x_y(0, 40));
        assert_eq!(range.end, SelectionCoordinate::x_y(usize::MAX, 41));

        // Direct line_around_in_logical_lines produces the exact same range
        let direct =
            SelectionRange::line_around_in_logical_lines(start, std::slice::from_ref(&logical));
        assert_eq!(direct, range);

        // BeforeZero coordinate also selects the entire physical line span
        let bz = SelectionCoordinate {
            x: SelectionX::BeforeZero,
            y: 40,
        };
        let (range_bz, pick_bz) = SelectionRange::smart_or_line_around_in_logical_lines(
            bz,
            std::slice::from_ref(&logical),
        );
        assert_eq!(pick_bz, None);
        assert_eq!(range_bz.start, SelectionCoordinate::x_y(0, 40));
        assert_eq!(range_bz.end, SelectionCoordinate::x_y(usize::MAX, 41));

        // Non-matching row falls back to start == end
        let wrong_row = SelectionCoordinate::x_y(0, 99);
        let (range_wr, pick_wr) = SelectionRange::smart_or_line_around_in_logical_lines(
            wrong_row,
            std::slice::from_ref(&logical),
        );
        assert_eq!(pick_wr, None);
        assert_eq!(
            range_wr,
            SelectionRange {
                start: wrong_row,
                end: wrong_row,
            }
        );
    }

    #[test]
    fn extreme_stable_row_index_checked_add_boundary() {
        let _mux = if mux::Mux::try_get().is_none() {
            let m = std::sync::Arc::new(mux::Mux::new(None));
            mux::Mux::set_mux(&m);
            Some(m)
        } else {
            None
        };
        let size = wezterm_term::TerminalSize {
            rows: 24,
            cols: 80,
            dpi: 96,
            pixel_width: 640,
            pixel_height: 384,
        };
        let (_tw, pane) =
            mux::termwiztermtab::allocate(size, std::sync::Arc::new(NativeAnchorTestConfig))
                .unwrap();

        // 1. Literal boundary negative: StableRowIndex::MAX.
        // start.y + 1 cannot be represented in StableRowIndex (isize);
        // checked_add returns None and all methods must return the fallback range
        // without panicking or attempting an invalid range query.
        let max_coord = SelectionCoordinate::x_y(0, StableRowIndex::MAX);
        let expected_fallback = SelectionRange {
            start: max_coord,
            end: max_coord,
        };

        assert_eq!(
            SelectionRange::line_around(max_coord, pane.as_ref()),
            expected_fallback
        );
        assert_eq!(
            SelectionRange::word_around(max_coord, pane.as_ref()),
            expected_fallback
        );
        assert_eq!(
            SelectionRange::smart_or_word_around(max_coord, pane.as_ref()),
            (expected_fallback, None)
        );
        assert_eq!(
            SelectionRange::smart_or_line_around(max_coord, pane.as_ref()),
            (expected_fallback, None)
        );

        // BeforeZero coordinate at StableRowIndex::MAX
        let max_bz = SelectionCoordinate {
            x: SelectionX::BeforeZero,
            y: StableRowIndex::MAX,
        };
        let expected_bz_fallback = SelectionRange {
            start: max_bz,
            end: max_bz,
        };
        assert_eq!(
            SelectionRange::line_around(max_bz, pane.as_ref()),
            expected_bz_fallback
        );
        assert_eq!(
            SelectionRange::word_around(max_bz, pane.as_ref()),
            expected_bz_fallback
        );
        assert_eq!(
            SelectionRange::smart_or_word_around(max_bz, pane.as_ref()),
            (expected_bz_fallback, None)
        );
        assert_eq!(
            SelectionRange::smart_or_line_around(max_bz, pane.as_ref()),
            (expected_bz_fallback, None)
        );

        // 2. Adjacent valid row positive: StableRowIndex::MAX - 1.
        // start.y.checked_add(1) evaluates to Some(StableRowIndex::MAX),
        // querying pane.get_logical_lines((MAX - 1)..MAX) cleanly without
        // integer overflow. Since no lines exist at this offset, all methods
        // return the fallback range.
        let adj_coord = SelectionCoordinate::x_y(0, StableRowIndex::MAX - 1);
        let expected_adj_fallback = SelectionRange {
            start: adj_coord,
            end: adj_coord,
        };

        assert_eq!(
            SelectionRange::line_around(adj_coord, pane.as_ref()),
            expected_adj_fallback
        );
        assert_eq!(
            SelectionRange::word_around(adj_coord, pane.as_ref()),
            expected_adj_fallback
        );
        assert_eq!(
            SelectionRange::smart_or_word_around(adj_coord, pane.as_ref()),
            (expected_adj_fallback, None)
        );
        assert_eq!(
            SelectionRange::smart_or_line_around(adj_coord, pane.as_ref()),
            (expected_adj_fallback, None)
        );

        // 3. Valid resident row positive: row 0 exists in the pane.
        // Confirms that non-extreme coordinates query the pane and produce
        // active selections.
        let valid_coord = SelectionCoordinate::x_y(0, 0);
        let valid_line = SelectionRange::line_around(valid_coord, pane.as_ref());
        assert_eq!(valid_line.start, SelectionCoordinate::x_y(0, 0));
        assert_eq!(valid_line.end, SelectionCoordinate::x_y(usize::MAX, 0));
    }

    #[test]
    fn line_around_saturated_endpoint_regression_at_max_first_row() {
        let p0: termwiz::surface::line::Line = "first line".into();
        let p1: termwiz::surface::line::Line = "second line".into();
        let full: termwiz::surface::line::Line = "first linesecond line".into();
        let logical = mux::pane::LogicalLine {
            physical_lines: vec![p0, p1],
            logical: full,
            first_row: StableRowIndex::MAX,
        };

        let start = SelectionCoordinate::x_y(0, StableRowIndex::MAX);
        let range =
            SelectionRange::line_around_in_logical_lines(start, std::slice::from_ref(&logical));

        assert_eq!(
            range.start,
            SelectionCoordinate::x_y(0, StableRowIndex::MAX)
        );
        assert_eq!(
            range.end,
            SelectionCoordinate::x_y(usize::MAX, StableRowIndex::MAX)
        );

        let (range_smart, pick) = SelectionRange::smart_or_line_around_in_logical_lines(
            start,
            std::slice::from_ref(&logical),
        );
        assert_eq!(pick, None);
        assert_eq!(range_smart, range);
    }

    #[test]
    fn process_hydrated_word_selection_single_char_and_multiline_unicode() {
        let size = wezterm_term::TerminalSize {
            rows: 24,
            cols: 20,
            dpi: 96,
            pixel_width: 160,
            pixel_height: 384,
        };
        let mut term = wezterm_term::Terminal::new(
            size,
            std::sync::Arc::new(NativeAnchorTestConfig),
            "selection-test",
            "test",
            Box::new(Vec::<u8>::new()),
        );
        term.advance_bytes(b"hello a https://example.com/\xe6\x97\xa5\xe6\x9c\xac\xe8\xaa\x9e world\r\nsecond line\r\n");

        let requested = 0..4;
        let plan = term.screen().capture_line_read(requested.clone()).unwrap();
        assert!(plan.starts_at_known_history_start());
        assert!(plan.lines().next().unwrap().last_cell_was_wrapped());
        let plans = Ok(vec![plan]);
        let cancelled = Arc::new(AtomicBool::new(false));

        // 1. Single character word "a" at col 6, row 0: MUST be preserved, not rejected
        let a_coord = SelectionCoordinate::x_y(6, 0);
        let payload_a = process_hydrated_word_line_selection(
            &plans,
            std::slice::from_ref(&requested),
            SelectionMode::Word,
            a_coord,
            None,
            &cancelled,
        )
        .expect("single character word selection must succeed");

        assert_eq!(payload_a.range.start, SelectionCoordinate::x_y(6, 0));
        assert_eq!(payload_a.range.end, SelectionCoordinate::x_y(6, 0));
        assert_eq!(payload_a.pick, None);

        // 2. Multiline Unicode URL: smart pick matches full Japanese URL
        let url_coord = SelectionCoordinate::x_y(10, 0);
        let payload_url = process_hydrated_word_line_selection(
            &plans,
            std::slice::from_ref(&requested),
            SelectionMode::Word,
            url_coord,
            None,
            &cancelled,
        )
        .expect("smart URL with Unicode characters must match");

        let pick = payload_url.pick.expect("should match URL pattern");
        assert_eq!(pick.kind, SelectionPatternKind::Url);
        assert_eq!(pick.text, "https://example.com/日本語");
        assert_eq!(payload_url.range.start, SelectionCoordinate::x_y(8, 0));
        // URL is 20 ASCII chars + 3 CJK double-width (6 cols) = 26 cols total.
        // Starts at 8 and crosses the 20-column physical row boundary.
        assert_eq!(payload_url.range.end, SelectionCoordinate::x_y(13, 1));

        // 3. Line mode (triple click) on the following logical line.
        let line_coord = SelectionCoordinate::x_y(3, 2);
        let payload_line = process_hydrated_word_line_selection(
            &plans,
            std::slice::from_ref(&requested),
            SelectionMode::Line,
            line_coord,
            None,
            &cancelled,
        )
        .expect("line selection must succeed");
        assert_eq!(payload_line.range.start, SelectionCoordinate::x_y(0, 2));
        assert_eq!(
            payload_line.range.end,
            SelectionCoordinate::x_y(usize::MAX, 2)
        );
    }

    #[test]
    fn process_hydrated_word_line_selection_declines_truncation_and_cancellation() {
        let size = wezterm_term::TerminalSize {
            rows: 24,
            cols: 10,
            dpi: 96,
            pixel_width: 80,
            pixel_height: 384,
        };
        let mut term = wezterm_term::Terminal::new(
            size,
            std::sync::Arc::new(NativeAnchorTestConfig),
            "selection-test",
            "test",
            Box::new(Vec::<u8>::new()),
        );
        // "0123456789wrappedline" wraps across 10-col boundary (rows 0 and 1)
        term.advance_bytes(b"0123456789wrappedline\r\n");

        // 1. Cancellation halts processing immediately
        let requested_full = 0..2;
        let plan_full = term
            .screen()
            .capture_line_read(requested_full.clone())
            .unwrap();
        let cancelled = Arc::new(AtomicBool::new(true));
        let cancel_res = process_hydrated_word_line_selection(
            &Ok(vec![plan_full]),
            std::slice::from_ref(&requested_full),
            SelectionMode::Word,
            SelectionCoordinate::x_y(2, 0),
            None,
            &cancelled,
        );
        assert!(cancel_res.is_err(), "cancelled read must return error");

        // 2. Forward truncation: snapshot covers only row 0, but line wraps into row 1
        let requested_fwd = 0..1;
        let plan_fwd = term
            .screen()
            .capture_line_read(requested_fwd.clone())
            .unwrap();
        let not_cancelled = Arc::new(AtomicBool::new(false));
        let fwd_res = process_hydrated_word_line_selection(
            &Ok(vec![plan_fwd]),
            std::slice::from_ref(&requested_fwd),
            SelectionMode::Word,
            SelectionCoordinate::x_y(2, 0),
            None,
            &not_cancelled,
        );
        assert!(
            fwd_res.is_err(),
            "forward truncated snapshot must be declined without fake fallback"
        );

        // 3. Backward truncation: snapshot requested covers row 1, but row 1 wrapped from row 0
        let requested_back = 1..2;
        let plan_back = term
            .screen()
            .capture_line_read(requested_back.clone())
            .unwrap();
        assert!(!plan_back.starts_at_known_history_start());
        let back_res = process_hydrated_word_line_selection(
            &Ok(vec![plan_back]),
            std::slice::from_ref(&requested_back),
            SelectionMode::Word,
            SelectionCoordinate::x_y(2, 1),
            None,
            &not_cancelled,
        );
        assert!(
            back_res.is_err(),
            "backward truncated snapshot must be declined without fake fallback"
        );
    }

    #[test]
    fn process_hydrated_word_selection_independent_bounded_endpoints() {
        let size = wezterm_term::TerminalSize {
            rows: 100,
            cols: 80,
            dpi: 96,
            pixel_width: 640,
            pixel_height: 1600,
        };
        let mut term = wezterm_term::Terminal::new(
            size,
            std::sync::Arc::new(NativeAnchorTestConfig),
            "selection-test",
            "test",
            Box::new(Vec::<u8>::new()),
        );
        // Write text at top and bottom
        let mut text = String::from("alpha_start beta\r\n");
        for _ in 0..70 {
            text.push_str("filler line\r\n");
        }
        text.push_str("gamma omega_end\r\n");
        term.advance_bytes(text.as_bytes());

        let req0 = 0..5;
        let req1 = 70..75;
        let plan0 = term.screen().capture_line_read(req0.clone()).unwrap();
        let plan1 = term.screen().capture_line_read(req1.clone()).unwrap();

        let plans = Ok(vec![plan0, plan1]);
        let requested_ranges = vec![req0, req1];
        let cancelled = Arc::new(AtomicBool::new(false));

        let start_coord = SelectionCoordinate::x_y(2, 0); // "alpha_start"
        let end_coord = SelectionCoordinate::x_y(8, 71); // "omega_end"

        let payload = process_hydrated_word_line_selection(
            &plans,
            &requested_ranges,
            SelectionMode::Word,
            start_coord,
            Some(end_coord),
            &cancelled,
        )
        .expect("independent bounded endpoint selection must succeed");

        assert_eq!(payload.range.start, SelectionCoordinate::x_y(0, 0));
        assert_eq!(payload.range.end, SelectionCoordinate::x_y(14, 71));
    }
}
