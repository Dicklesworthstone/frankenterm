use crate::selection::{
    Selection, SelectionAuthority, SelectionCoordinate, SelectionMode, SelectionRange, SelectionX,
    SmartSelectionPick,
};
use crate::smart_selection_a11y::emit_smart_selection_pick;
use mux::pane::{LogicalLine, Pane, PaneId};
use std::cell::RefMut;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use termwiz::surface::Line;
use wezterm_term::StableRowIndex;
use window::WindowOps;

/// One bounded clipboard transaction, independent of renderer cache capacity.
/// The 64 MiB text cap and fixed deadline never reset as chunks arrive.
#[derive(Debug)]
pub(crate) struct SelectionCopy {
    source_sequence: termwiz::surface::SequenceNo,
    next_row: StableRowIndex,
    end_row: StableRowIndex,
    selection: SelectionRange,
    rectangular: bool,
    pending_line: Option<(StableRowIndex, Line)>,
    text: String,
    join_previous: bool,
    has_line: bool,
    deadline: std::time::Instant,
    deadline_wake: Option<SelectionCopyDeadline>,
    local: bool,
    local_read: Option<LocalSelectionRead>,
}

#[derive(Debug)]
struct SelectionCopyDeadline(futures::future::AbortHandle);

impl Drop for SelectionCopyDeadline {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn expire_selection_copy_deadline(
    pending: &mut Option<crate::selection::PendingNativeSelection>,
    deadline: std::time::Instant,
    now: std::time::Instant,
) -> bool {
    if pending
        .as_ref()
        .and_then(|pending| pending.text_copy.as_ref())
        .is_some_and(|copy| copy.deadline() == deadline)
    {
        crate::selection::PendingNativeSelection::expire_text_copy(pending, now)
    } else {
        false
    }
}

type SelectionReadPlans = anyhow::Result<Vec<wezterm_term::screen::ScreenLineRead>>;

/// Return hydrated rows to the admitted worker for destruction, including
/// cancellation while the result is queued. The worker retains its permit.
struct LocalSelectionReadReady {
    plans: Option<SelectionReadPlans>,
    retire: SyncSender<SelectionReadPlans>,
}

impl Drop for LocalSelectionReadReady {
    fn drop(&mut self) {
        if let Some(plans) = self.plans.take() {
            let _ = self.retire.send(plans);
        }
    }
}

struct LocalSelectionRead {
    receiver: Receiver<LocalSelectionReadReady>,
    ready: Option<LocalSelectionReadReady>,
    cancelled: Arc<AtomicBool>,
}

impl std::fmt::Debug for LocalSelectionRead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalSelectionRead")
            .field("ready", &self.ready.is_some())
            .finish_non_exhaustive()
    }
}

impl Drop for LocalSelectionRead {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

impl LocalSelectionRead {
    fn start(
        capture: impl FnOnce() -> Option<anyhow::Result<wezterm_term::screen::ScreenLineRead>>,
        deadline: std::time::Instant,
        wake: impl FnOnce() + Send + 'static,
    ) -> Result<Option<Self>, &'static str> {
        let Some(permit) = mux::pane::LineReadPermit::try_acquire() else {
            return Ok(None);
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let (sender, receiver) = sync_channel(1);
        // Reserve and start before capturing any source allocations.
        let worker = permit
            .start(
                move || {
                    worker_cancelled.load(Ordering::Acquire)
                        || std::time::Instant::now() >= deadline
                },
                move |plans, permit| {
                    let (retire, retired) = sync_channel(1);
                    let ready = LocalSelectionReadReady {
                        plans: Some(plans),
                        retire,
                    };
                    if sender.send(ready).is_ok() {
                        wake();
                        // Admission covers queued payload and its destruction.
                        // Only this worker waits for the UI retirement guard.
                        drop(retired.recv());
                    }
                    drop(permit);
                },
            )
            .map_err(|_| "The text reader could not start. Copy the selection again.")?;
        let Some(plan) = capture() else {
            return Err("This pane cannot provide a bounded text read.");
        };
        let Ok(plan) = plan else {
            return Ok(None);
        };
        let read = Self {
            receiver,
            ready: None,
            cancelled,
        };
        worker.submit(vec![plan]);
        Ok(Some(read))
    }
}

impl SelectionCopy {
    const MAX_BYTES: usize = 64 * 1024 * 1024;

    pub(crate) fn deadline(&self) -> std::time::Instant {
        self.deadline
    }

    pub(crate) fn wake_at(&self) -> std::time::Instant {
        if self.local {
            // Admission and terminal locks are nonblocking. A finite 20 Hz
            // retry also covers a quiet pane when another read owns the pool.
            self.deadline
                .min(std::time::Instant::now() + std::time::Duration::from_millis(50))
        } else {
            self.deadline
        }
    }

    fn verify_source(&self, sequence: termwiz::surface::SequenceNo) -> Result<(), &'static str> {
        if std::time::Instant::now() >= self.deadline {
            return Err("The selected text did not arrive in time. Copy the selection again.");
        }
        if sequence != self.source_sequence {
            return Err("The pane changed while copying. Copy the selection again.");
        }
        Ok(())
    }

    fn finish(
        &mut self,
        sequence: termwiz::surface::SequenceNo,
    ) -> Result<Option<String>, &'static str> {
        self.verify_source(sequence)?;
        if self.next_row != self.end_row {
            return Ok(None);
        }
        Ok(Some(std::mem::take(&mut self.text)))
    }

    fn new(selection: &Selection, source_sequence: termwiz::surface::SequenceNo) -> Option<Self> {
        let selection_range = selection.range?.normalize();
        let end_row = selection_range.end.y.checked_add(1)?;
        Some(Self {
            source_sequence,
            next_row: selection_range.start.y,
            end_row,
            selection: selection_range,
            rectangular: selection.rectangular,
            pending_line: None,
            text: String::new(),
            join_previous: false,
            has_line: false,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(30),
            deadline_wake: None,
            local: false,
            local_read: None,
        })
    }

    fn append_span(
        &mut self,
        row: StableRowIndex,
        line: Line,
        next: Option<StableRowIndex>,
    ) -> Result<bool, &'static str> {
        // Refuse before materializing a second copy of an oversized row.
        line.visible_cells()
            .try_fold(0usize, |bytes, cell| {
                bytes
                    .checked_add(cell.str().len())
                    .filter(|bytes| *bytes <= Self::MAX_BYTES)
            })
            .ok_or("selection exceeds the 64 MiB copy limit")?;
        let (span, continues, ends_before) =
            selected_line_span(&line, row, next, self.selection, self.rectangular);
        let text = span.as_str();
        let newline = self.has_line && !self.join_previous;
        let additional = text
            .len()
            .checked_add(usize::from(newline))
            .ok_or("selection exceeds the copy limit")?;
        let total = self
            .text
            .len()
            .checked_add(additional)
            .filter(|total| *total <= Self::MAX_BYTES)
            .ok_or("selection exceeds the 64 MiB copy limit")?;
        if total > self.text.capacity() {
            let capacity = total
                .max(self.text.capacity().saturating_mul(2))
                .min(Self::MAX_BYTES);
            self.text
                .try_reserve_exact(capacity - self.text.len())
                .map_err(|_| "selection copy allocation failed")?;
        }
        if newline {
            self.text.push('\n');
        }
        self.text.push_str(&text);
        self.has_line = true;
        self.join_previous = continues;
        Ok(ends_before)
    }

    fn push_chunk(&mut self, rows: Vec<Line>) -> Result<(), &'static str> {
        for line in rows {
            let row = self.next_row;
            self.next_row = row.checked_add(1).ok_or("selection row overflow")?;
            if let Some((previous_row, previous)) = self.pending_line.take() {
                if self.append_span(previous_row, previous, Some(row))? {
                    // This was the empty BeforeZero endpoint, not another line.
                    continue;
                }
            }
            self.pending_line = Some((row, line));
        }
        if self.next_row == self.end_row {
            if let Some((row, line)) = self.pending_line.take() {
                self.append_span(row, line, None)?;
            }
        }
        Ok(())
    }
}

/// Emit the AT-tree announcement for a picked smart-selection span.
/// Called from the `SelectionMode::Word` and `SelectionMode::Line`
/// mouse-handler branches after `smart_or_word_around` /
/// `smart_or_line_around` resolves to a smart pattern. No-op when
/// the legacy word- / line-boundary fallback fired (pick is `None`)
/// so screen readers stay quiet on plain word / line picks
/// (ft-cnil8.4 / ft-weglh / ft-t5j0a).
fn announce_pick_if_smart(pick: Option<SmartSelectionPick>) {
    if let Some(p) = pick {
        emit_smart_selection_pick(p.kind, &p.text);
    }
}

impl super::TermWindow {
    /// Clipboard ownership must expire even when a hidden or failed surface
    /// never presents another frame. One cancellable wake belongs to each copy.
    fn arm_selection_copy_deadline(
        &self,
        pane_id: PaneId,
        copy: &mut SelectionCopy,
    ) -> Result<(), &'static str> {
        let window = self
            .window
            .clone()
            .ok_or("The window closed before copying could complete.")?;
        let reservation = match promise::spawn::try_reserve_main_thread(
            promise::spawn::MainThreadServiceClass::Input,
            8 * 1024,
        ) {
            promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => reservation,
            _ => return Err("The copy deadline could not be scheduled. Copy the selection again."),
        };
        let deadline = copy.deadline();
        let (abort, registration) = futures::future::AbortHandle::new_pair();
        copy.deadline_wake = Some(SelectionCopyDeadline(abort));
        reservation
            .spawn_local(async move {
                let _ = futures::future::Abortable::new(
                    async move {
                        promise::spawn::sleep(
                            deadline.saturating_duration_since(std::time::Instant::now()),
                        )
                        .await;
                        window.notify(super::TermWindowNotif::Apply(Box::new(move |tw| {
                            let expired = tw
                                .pane_state
                                .borrow_mut()
                                .get_mut(&pane_id)
                                .is_some_and(|state| {
                                    expire_selection_copy_deadline(
                                        &mut state.pending_native_selection,
                                        deadline,
                                        std::time::Instant::now(),
                                    )
                                });
                            if expired {
                                frankenterm_toast_notification::persistent_toast_notification(
                                    "Selection was not copied",
                                    "The selected text did not arrive in time. Copy the selection again.",
                                );
                            }
                        })));
                    },
                    registration,
                )
                .await;
            })
            .detach();
        Ok(())
    }

    pub fn selection_frame_stamp(
        &self,
        pane: &Arc<dyn Pane>,
    ) -> Option<crate::selection::SelectionFrameStamp> {
        let pos = self
            .get_panes_to_render()
            .into_iter()
            .find(|pos| Arc::ptr_eq(&pos.pane, pane))?;
        self.selection_frame_stamp_for_position(pane, &pos)
    }

    pub fn selection_frame_stamp_for_position(
        &self,
        pane: &Arc<dyn Pane>,
        pos: &mux::tab::PositionedPane,
    ) -> Option<crate::selection::SelectionFrameStamp> {
        if !Arc::ptr_eq(&pos.pane, pane) {
            return None;
        }
        let (authority, source_sequence, dims) = SelectionAuthority::capture_source(&**pane)?;
        let stamp = crate::selection::SelectionFrameStamp {
            authority,
            source_sequence,
            viewport: self
                .get_viewport(pane.pane_id())
                .unwrap_or(dims.physical_top),
            geometry: self.selection_frame_geometry(pos)?,
        };
        SelectionAuthority::capture_source(&**pane)
            .filter(|(after, _, after_dims)| *after == authority && *after_dims == dims)
            .map(|_| stamp)
    }

    pub fn selection_frame_geometry(&self, pos: &mux::tab::PositionedPane) -> Option<[usize; 12]> {
        let (padding_left, padding_top) = self.padding_left_top();
        let border = self.get_os_border();
        let top_bar = if self.show_tab_bar && !self.config.tab_bar_at_bottom {
            self.tab_bar_pixel_height().ok()?
        } else {
            0.0
        };
        Some([
            self.render_metrics.cell_size.width as usize,
            self.render_metrics.cell_size.height as usize,
            pos.left,
            pos.top,
            pos.width,
            pos.height,
            self.dimensions.pixel_width,
            self.dimensions.pixel_height,
            (padding_left + border.left.get() as f32).to_bits() as usize,
            (padding_top + top_bar + border.top.get() as f32).to_bits() as usize,
            self.shape_generation,
            self.config.generation() as usize,
        ])
    }

    fn mouse_selection_authority(&self, pane: &Arc<dyn Pane>) -> Option<SelectionAuthority> {
        let current = self.selection_frame_stamp(pane);
        let state = self.pane_state(pane.pane_id());
        let mouse = state.mouse_selection_frame?;
        (state.selection_frame.for_mouse(current) == Some(mouse)).then_some(mouse.authority)
    }
    pub fn selection(&self, pane_id: PaneId) -> RefMut<'_, Selection> {
        RefMut::map(self.pane_state(pane_id), |state| &mut state.selection)
    }

    pub fn update_selection(
        &mut self,
        pane: &Arc<dyn Pane>,
        expected: Option<SelectionAuthority>,
        update: impl FnOnce(&mut Selection),
    ) {
        let pane_id = pane.pane_id();
        let current_seqno = pane.get_current_seqno();
        let mut selection = self.selection(pane_id).clone();
        {
            update(&mut selection);
            selection.seqno = current_seqno;
            selection.authority = expected;
            if expected.is_none()
                || selection.is_invalidated_by(SelectionAuthority::capture(&**pane))
            {
                selection.clear();
            }
        }
        self.commit_selection_candidate(pane, selection);
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    pub fn selection_authority_is_current(&self, pane: &Arc<dyn Pane>) -> bool {
        let current = SelectionAuthority::capture(&**pane);
        self.synchronize_native_selection(pane, current);
        self.selection(pane.pane_id()).is_authorized_by(current)
    }

    pub fn selection_authority_has_changed(&self, pane: &Arc<dyn Pane>) -> bool {
        let current = SelectionAuthority::capture(&**pane);
        !self.synchronize_native_selection(pane, current)
            && self.selection(pane.pane_id()).is_invalidated_by(current)
    }

    fn capture_native_selection(
        pane: &Arc<dyn Pane>,
        local: &mux::localpane::LocalPane,
        desired: &Selection,
    ) -> crate::selection::NativeSelectionCapture {
        use crate::selection::NativeSelectionCapture;
        let Some((authority, _, dimensions)) = SelectionAuthority::capture_source(&**pane) else {
            return NativeSelectionCapture::Busy;
        };
        if desired.authority != Some(authority) {
            return NativeSelectionCapture::Invalidated;
        }
        local
            .capture_selection_anchor(
                authority.layout_floor(),
                desired.seqno,
                dimensions,
                desired.native_points(),
            )
            .into()
    }

    fn commit_selection_candidate(&self, pane: &Arc<dyn Pane>, desired: Selection) {
        if pane.downcast_ref::<mux::localpane::LocalPane>().is_none()
            || desired.rectangular
            || (desired.origin.is_none() && desired.range.is_none())
        {
            let mut state = self.pane_state(pane.pane_id());
            state.pending_native_selection = None;
            state.selection = desired;
            return;
        }
        // A newer gesture supersedes any deferred clipboard operation.
        self.pane_state(pane.pane_id()).pending_native_selection =
            Some(crate::selection::PendingNativeSelection::new(desired));
        self.retry_pending_native_selection(pane);
    }

    pub(super) fn retry_pending_native_selection(&self, pane: &Arc<dyn Pane>) {
        let Some(mut pending) = self
            .pane_state(pane.pane_id())
            .pending_native_selection
            .take()
        else {
            return;
        };
        if pending
            .text_copy
            .as_ref()
            .is_some_and(|copy| std::time::Instant::now() >= copy.deadline())
        {
            frankenterm_toast_notification::persistent_toast_notification(
                "Selection was not copied",
                "The selected text did not arrive in time. Copy the selection again.",
            );
            return;
        }
        if pending.committed {
            self.selection_authority_is_current(pane);
            let current = self.selection(pane.pane_id()).clone();
            let same = match (current.native_anchor(), pending.desired.native_anchor()) {
                (Some(current), Some(expected)) => current == expected,
                (None, None) => current == pending.desired,
                _ => false,
            };
            if !same {
                self.pane_state(pane.pane_id()).pending_native_selection = None;
                return;
            }
            pending.desired = current;
        }
        let local = pane.downcast_ref::<mux::localpane::LocalPane>();
        let capture = if let Some(local) = local {
            Self::capture_native_selection(pane, local, &pending.desired)
        } else {
            match SelectionAuthority::capture(&**pane) {
                None => crate::selection::NativeSelectionCapture::Busy,
                Some(authority) if pending.desired.authority == Some(authority) => {
                    crate::selection::NativeSelectionCapture::Unremappable
                }
                Some(_) => crate::selection::NativeSelectionCapture::Invalidated,
            }
        };
        let result = {
            let mut state = self.pane_state(pane.pane_id());
            let result = pending.try_commit(&mut state.selection, capture);
            if matches!(
                result,
                Some(crate::selection::NativeSelectionCommit::Applied { .. })
            ) && pending.copy.is_some()
            {
                pending.committed = true;
                pending.desired = state.selection.clone();
            } else if result.is_some() {
                state.pending_native_selection = None;
            }
            result
        };
        if result.is_none() {
            self.pane_state(pane.pane_id()).pending_native_selection = Some(pending);
            return;
        }
        if let Some(crate::selection::NativeSelectionCommit::Applied { needs_repaint }) = result {
            if needs_repaint {
                if let Some(window) = self.window.as_ref() {
                    window.invalidate();
                }
            }
            if let Some(destination) = pending.copy {
                if local.is_some()
                    || pane
                        .downcast_ref::<frankenterm_client::pane::ClientPane>()
                        .is_some()
                {
                    match self.advance_selection_copy(pane, &mut pending) {
                        Ok(Some(text)) => {
                            if local.is_none() || !text.is_empty() {
                                self.copy_to_clipboard(destination, text);
                            }
                        }
                        Ok(None) => {
                            self.pane_state(pane.pane_id()).pending_native_selection =
                                Some(pending);
                        }
                        Err(reason) => {
                            frankenterm_toast_notification::persistent_toast_notification(
                                "Selection was not copied",
                                reason,
                            )
                        }
                    }
                    return;
                }
            }
        }
    }

    fn advance_local_selection_read(
        &self,
        pane: &Arc<dyn Pane>,
        copy: &mut SelectionCopy,
        sequence: termwiz::surface::SequenceNo,
        dimensions: mux::renderable::RenderableDimensions,
        end: StableRowIndex,
    ) -> Result<Option<Vec<Line>>, &'static str> {
        copy.local = true;
        let requested = copy.next_row..end;
        if copy.local_read.is_none() {
            let window = self.window.clone();
            copy.local_read = LocalSelectionRead::start(
                || pane.capture_line_read(requested.clone(), &mut Default::default()),
                copy.deadline,
                move || {
                    if let Some(window) = window {
                        window.invalidate();
                    }
                },
            )?;
            return Ok(None);
        }
        let read = copy.local_read.as_mut().unwrap();
        if read.ready.is_none() {
            match read.receiver.try_recv() {
                Ok(ready) => read.ready = Some(ready),
                Err(TryRecvError::Empty) => return Ok(None),
                Err(TryRecvError::Disconnected) => {
                    return Err("The text reader stopped. Copy the selection again.");
                }
            }
        }
        let plans = read
            .ready
            .as_ref()
            .unwrap()
            .plans
            .as_ref()
            .unwrap()
            .as_ref()
            .map_err(|_| "The selected history could not be loaded. Copy the selection again.")?;
        if plans.len() != 1 {
            return Err("The text reader returned an incomplete selection.");
        }
        let mut rows = None;
        let published = pane.publish_line_reads_at_layout(plans, sequence, dimensions, &mut || {
            let mut bytes = wezterm_term::screen::ScreenLineRead::MAX_PAYLOAD_BYTES;
            let mut work = 65_536;
            rows =
                plans[0].try_clone_viewport_for_snapshot(requested.clone(), &mut bytes, &mut work);
        });
        if !published {
            return Ok(None);
        }
        let (first, rows) = rows.ok_or("The selected rows exceed the text read budget.")?;
        if first != requested.start
            || usize::try_from(requested.end - requested.start).ok() != Some(rows.len())
        {
            return Err("The selected history changed or is unavailable. Select it again.");
        }
        // Returning the ready object retires heavy hydrated state on its worker.
        copy.local_read = None;
        Ok(Some(rows))
    }

    fn advance_selection_copy(
        &self,
        pane: &Arc<dyn Pane>,
        pending: &mut crate::selection::PendingNativeSelection,
    ) -> Result<Option<String>, &'static str> {
        use frankenterm_client::pane::SelectionReadError;
        let Some((authority, sequence, dimensions)) = SelectionAuthority::capture_source(&**pane)
        else {
            return Ok(None);
        };
        if pending.desired.authority != Some(authority) {
            return Err("The pane changed. Select the text again to copy it.");
        }
        if pending.text_copy.is_none() {
            let mut copy = SelectionCopy::new(&pending.desired, sequence)
                .ok_or("The selection range is unavailable. Select the text again.")?;
            self.arm_selection_copy_deadline(pane.pane_id(), &mut copy)?;
            pending.text_copy = Some(copy);
        }
        let copy = pending.text_copy.as_mut().unwrap();
        copy.verify_source(sequence)?;
        if copy.next_row < copy.end_row {
            let end = copy
                .next_row
                .checked_add(64)
                .unwrap_or(copy.end_row)
                .min(copy.end_row);
            let rows = if let Some(client) =
                pane.downcast_ref::<frankenterm_client::pane::ClientPane>()
            {
                client.selection_lines(
                    authority.layout_floor(),
                    sequence,
                    pending.desired.seqno,
                    copy.next_row..end,
                )
            } else {
                match self.advance_local_selection_read(pane, copy, sequence, dimensions, end)? {
                    Some(rows) => Ok(rows),
                    None => return Ok(None),
                }
            };
            match rows {
                Ok(rows) => copy.push_chunk(rows)?,
                Err(SelectionReadError::Busy) => return Ok(None),
                Err(SelectionReadError::TooLarge) => {
                    return Err("The selection exceeds the 64 MiB copy limit. Select less text.");
                }
                Err(SelectionReadError::SourceChanged | SelectionReadError::InvalidRange) => {
                    return Err(
                        "The selected text changed or is unavailable. Select it again to copy it.",
                    );
                }
            }
        }
        if copy.next_row < copy.end_row {
            // Actual bounded progress, not an idle retry: one chunk per frame.
            if let Some(window) = self.window.as_ref() {
                window.invalidate();
            }
            return Ok(None);
        }
        match SelectionAuthority::capture_source(&**pane) {
            None => Ok(None),
            Some((current, current_sequence, _))
                if current == authority && current_sequence == sequence =>
            {
                copy.finish(current_sequence)
            }
            Some(_) => Err("The pane changed while copying. Copy the selection again."),
        }
    }

    /// Bind release to the exact pending endpoint; never copy an older anchor.
    pub(super) fn defer_pending_selection_copy(
        &self,
        pane: &Arc<dyn Pane>,
        destination: config::keyassignment::ClipboardCopyDestination,
    ) -> bool {
        let mut state = self.pane_state(pane.pane_id());
        if let Some(pending) = state.pending_selection_start.as_mut() {
            pending.released = true;
            pending.copy = Some(destination);
            pending.paint_retries_remaining = 3;
            drop(state);
            if let Some(window) = self.window.as_ref() {
                window.invalidate();
            }
            return true;
        }
        if state.pending_native_selection.is_none()
            && (pane.downcast_ref::<mux::localpane::LocalPane>().is_some()
                || pane
                    .downcast_ref::<frankenterm_client::pane::ClientPane>()
                    .is_some())
            && state.selection.range.is_some()
        {
            let mut pending =
                crate::selection::PendingNativeSelection::new(state.selection.clone());
            pending.committed = true;
            state.pending_native_selection = Some(pending);
        }
        let Some(pending) = state.pending_native_selection.as_mut() else {
            return false;
        };
        pending.copy = Some(destination);
        pending.paint_retries_remaining = 3;
        drop(state);
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
        true
    }

    /// Return true only when a potentially valid remap is temporarily
    /// unavailable or has outrun the caller's frame. Keep its anchor, but do
    /// not authorize old coordinates for painting, text reads or new motion.
    pub(super) fn synchronize_native_selection(
        &self,
        pane: &Arc<dyn Pane>,
        current: Option<SelectionAuthority>,
    ) -> bool {
        if !self.selection(pane.pane_id()).is_invalidated_by(current) {
            return false;
        }
        let Some(local) = pane.downcast_ref::<mux::localpane::LocalPane>() else {
            return false;
        };
        let Some(token) = self.selection(pane.pane_id()).native_anchor().cloned() else {
            return false;
        };
        let Some((floor, sequence, dimensions, points)) = local.selection_anchor_snapshot(&token)
        else {
            return true;
        };
        let resolved = SelectionAuthority::from_native_snapshot(&**pane, floor, dimensions);
        if resolved != current {
            return true;
        }
        if let Some((points, authority)) = points.zip(resolved) {
            self.selection(pane.pane_id())
                .rebase_native_anchor(points, authority, sequence);
        }
        false
    }

    /// Returns the selection region as a series of Line
    pub fn selection_lines(&self, pane: &Arc<dyn Pane>) -> Vec<Line> {
        let expected = SelectionAuthority::capture(&**pane);
        self.synchronize_native_selection(pane, expected);
        if !self.selection(pane.pane_id()).is_authorized_by(expected) {
            return Vec::new();
        }
        let rectangular = self.selection(pane.pane_id()).rectangular;
        let result = if let Some(sel) = self
            .selection(pane.pane_id())
            .range
            .as_ref()
            .map(|r| r.normalize())
        {
            selected_lines_from_logical_lines(&pane.get_logical_lines(sel.rows()), sel, rectangular)
        } else {
            Vec::new()
        };

        if SelectionAuthority::capture(&**pane) != expected {
            return Vec::new();
        }
        result
    }

    /// Returns the selection text only
    pub fn selection_text(&self, pane: &Arc<dyn Pane>) -> String {
        self.try_selection_text(pane).unwrap_or_default()
    }

    /// An unavailable source requires retry; acquired empty text is complete.
    fn try_selection_text(&self, pane: &Arc<dyn Pane>) -> Option<String> {
        let expected = SelectionAuthority::capture(&**pane);
        self.synchronize_native_selection(pane, expected);
        if expected.is_none() || !self.selection(pane.pane_id()).is_authorized_by(expected) {
            return None;
        }
        let (rectangular, sel) = {
            let selection = self.selection(pane.pane_id());
            let Some(sel) = selection.range.as_ref().map(|r| r.normalize()) else {
                return Some(String::new());
            };
            (selection.rectangular, sel)
        };

        let text =
            selected_text_from_logical_lines(&pane.get_logical_lines(sel.rows()), sel, rectangular);
        if SelectionAuthority::capture(&**pane) != expected {
            return None;
        }
        Some(text)
    }

    pub fn clear_selection_drag(&mut self) {
        self.active_selection_drag_pane = None;
        self.active_selection_drag_button = None;
    }

    pub fn begin_selection_drag(&mut self, pane: &Arc<dyn Pane>) {
        self.active_selection_drag_button = self.current_mouse_event.as_ref().and_then(|event| {
            super::mouseevent::selection_gesture_button(&event.kind, &self.current_mouse_buttons)
        });
        self.active_selection_drag_pane = self.active_selection_drag_button.map(|_| pane.pane_id());
    }

    pub fn clear_selection(&mut self, pane: &Arc<dyn Pane>) {
        self.clear_selection_drag();
        self.pane_state(pane.pane_id()).pending_selection_start = None;
        self.pane_state(pane.pane_id()).pending_native_selection = None;
        self.update_selection(pane, None, Selection::clear);
    }

    pub fn extend_selection_at_mouse_cursor(&mut self, mode: SelectionMode, pane: &Arc<dyn Pane>) {
        self.extend_selection_at_position(mode, pane, None);
    }

    fn extend_selection_at_position(
        &mut self,
        mode: SelectionMode,
        pane: &Arc<dyn Pane>,
        retained: Option<(
            SelectionAuthority,
            wezterm_term::input::ClickPosition,
            StableRowIndex,
        )>,
    ) -> bool {
        // Even a deferred first motion is a drag, not a hyperlink click.
        self.pane_state(pane.pane_id()).suppress_selection_link = true;
        let had_pending_start = self
            .pane_state(pane.pane_id())
            .pending_selection_start
            .is_some();
        self.retry_pending_selection_start(pane);
        if had_pending_start {
            // A successful retry already applies the retained motion once.
            // Real input also restarts presentation after background retries
            // exhaust; this is one invalidation per motion, not a paint loop.
            if self
                .pane_state(pane.pane_id())
                .pending_selection_start
                .is_some()
            {
                if let Some(window) = self.window.as_ref() {
                    window.invalidate();
                }
            }
            return false;
        }
        self.retry_pending_native_selection(pane);
        self.selection_authority_is_current(pane);
        let pending_desired = self
            .pane_state(pane.pane_id())
            .pending_native_selection
            .as_ref()
            .map(|pending| pending.desired.clone());
        let mut desired = pending_desired.unwrap_or_else(|| self.selection(pane.pane_id()).clone());
        let current = SelectionAuthority::capture(&**pane);
        if desired.is_invalidated_by(current) {
            self.clear_selection(pane);
            return false;
        }
        if !desired.is_authorized_by(current)
            || retained
                .map(|r| r.0)
                .or_else(|| self.mouse_selection_authority(pane))
                .is_none()
        {
            // Output/parser contention or autoscroll can outrun presentation.
            // Wait for a usable frame without discarding the drag's anchor.
            if retained.is_none() {
                let mut state = self.pane_state(pane.pane_id());
                if let (Some(frame), Some(coordinate), Some(end), Some(button)) = (
                    state.mouse_selection_frame,
                    desired.origin,
                    state.mouse_terminal_coords,
                    self.active_selection_drag_button,
                ) {
                    if desired.authority == Some(frame.authority) {
                        state.pending_selection_start =
                            Some(crate::selection::PendingSelectionStart {
                                frame,
                                coordinate,
                                mode,
                                button,
                                paint_retries_remaining: 3,
                                released: false,
                                end: Some(end),
                                copy: None,
                            });
                    }
                }
            }
            if let Some(window) = self.window.as_ref() {
                window.invalidate();
            }
            return false;
        }
        desired.seqno = pane.get_current_seqno();
        let (position, y) = match retained
            .map(|(_, position, row)| (position, row))
            .or(self.pane_state(pane.pane_id()).mouse_terminal_coords)
        {
            Some(coords) => coords,
            None => return false,
        };
        let x = position.column;
        match mode {
            SelectionMode::Cell | SelectionMode::Block => {
                // Origin is the cell in which the selection action started. E.g. the cell
                // that had the mouse over it when the left mouse button was pressed
                let origin = desired.origin.unwrap_or(SelectionCoordinate::x_y(x, y));
                desired.origin = Some(origin);
                desired.rectangular = mode == SelectionMode::Block;

                // Compute the start and end horizontall cell of the selection.
                // The selection extent depends on the mouse cursor position in relation
                // to the origin.
                let (start_x, end_x) = if mode == SelectionMode::Block {
                    if x >= origin.x {
                        // If the selection is extending forwards from the origin,
                        // it includes the origin
                        (origin.x, SelectionX::Cell(x).saturating_sub(1))
                    } else {
                        // If the selection is extending backwards from the origin,
                        // it doesn't include the origin
                        (origin.x.saturating_sub(1), SelectionX::Cell(x))
                    }
                } else {
                    if (x >= origin.x && y == origin.y) || y > origin.y {
                        // If the selection is extending forwards from the origin, it includes the
                        // origin and doesn't include the cell under the cursor. Note that the
                        // reported cell here is offset by -50% from the real cell you see on the
                        // screen, so this causes a visual cell on the screen to be selected when
                        // the mouse moves over 50% of its width, which effectively means the next
                        // cell is being reported here, hence it's excluded
                        (origin.x, SelectionX::Cell(x).saturating_sub(1))
                    } else {
                        // If the selection is extending backwards from the origin, it doesn't
                        // include the origin and includes the cell under the cursor, which has
                        // the same effect as described above when going backwards
                        (origin.x.saturating_sub(1), SelectionX::Cell(x))
                    }
                };

                desired.range = if mode == SelectionMode::Block && origin.x == x {
                    // Ignore rectangle selections with a width of zero
                    None
                } else if origin.x != x || origin.y != y {
                    // Only considers a selection if the cursor moved from the origin point
                    Some(
                        SelectionRange::start(SelectionCoordinate {
                            x: start_x,
                            y: origin.y,
                        })
                        .extend(SelectionCoordinate { x: end_x, y }),
                    )
                } else {
                    None
                };
            }
            SelectionMode::Word => {
                let (end_word, end_pick) =
                    SelectionRange::smart_or_word_around(SelectionCoordinate::x_y(x, y), &**pane);

                let start_coord = desired.origin.clone().unwrap_or(end_word.start);
                // Anchor-side pick is intentionally discarded: the
                // user gets one selection, and the announcement
                // tracks the cursor (moving endpoint) so screen
                // readers don't double-fire on drag.
                let (start_word, _) = SelectionRange::smart_or_word_around(start_coord, &**pane);

                let selection_range = start_word.extend_with(end_word);
                desired.range = Some(selection_range);
                desired.rectangular = false;
                announce_pick_if_smart(end_pick);
            }
            SelectionMode::Line => {
                let (end_line, end_pick) =
                    SelectionRange::smart_or_line_around(SelectionCoordinate::x_y(x, y), &**pane);

                let start_coord = desired.origin.clone().unwrap_or(end_line.start);
                // Anchor-side pick is intentionally discarded so a
                // drag-select doesn't double-fire the announcement;
                // the cursor (moving endpoint) drives the AT cue.
                let (start_line, _) = SelectionRange::smart_or_line_around(start_coord, &**pane);

                let selection_range = start_line.extend_with(end_line);
                desired.range = Some(selection_range);
                desired.rectangular = false;
                announce_pick_if_smart(end_pick);
            }
            SelectionMode::SemanticZone => {
                let end_word = SelectionRange::zone_around(SelectionCoordinate::x_y(x, y), &**pane);

                let start_coord = desired.origin.clone().unwrap_or(end_word.start);
                let start_word = SelectionRange::zone_around(start_coord, &**pane);

                let selection_range = start_word.extend_with(end_word);
                desired.range = Some(selection_range);
                desired.rectangular = false;
            }
        }

        if desired.is_invalidated_by(SelectionAuthority::capture(&**pane)) {
            self.clear_selection(pane);
            return false;
        }
        self.commit_selection_candidate(pane, desired);
        let dims = pane.get_dimensions();

        // Scroll viewport when the mouse moves out of its vertical bounds.
        if position.row == 0 && position.y_pixel_offset < 0 {
            self.set_viewport(pane.pane_id(), Some(y.saturating_sub(1)), dims);
        } else if position.row >= dims.viewport_rows as i64 {
            let top = self
                .get_viewport(pane.pane_id())
                .unwrap_or(dims.physical_top);
            self.set_viewport(pane.pane_id(), Some(top.saturating_add(1)), dims);
        }

        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
        true
    }

    pub fn select_text_at_mouse_cursor(&mut self, mode: SelectionMode, pane: &Arc<dyn Pane>) {
        {
            let mut state = self.pane_state(pane.pane_id());
            state.pending_selection_start = None;
            state.pending_native_selection = None;
            state.suppress_selection_link = false;
        }
        let expected = self.mouse_selection_authority(pane);
        if expected.is_none() {
            let pending = {
                let state = self.pane_state(pane.pane_id());
                state
                    .mouse_selection_frame
                    .zip(state.mouse_terminal_coords)
                    .zip(self.active_selection_drag_button)
                    .map(|((frame, (position, row)), button)| {
                        crate::selection::PendingSelectionStart {
                            frame,
                            coordinate: SelectionCoordinate::x_y(position.column, row),
                            mode,
                            button,
                            paint_retries_remaining: 3,
                            released: false,
                            end: None,
                            copy: None,
                        }
                    })
            };
            self.selection(pane.pane_id()).clear();
            let mut state = self.pane_state(pane.pane_id());
            state.pending_selection_start = pending;
            state.suppress_selection_link = true;
            if let Some(window) = self.window.as_ref() {
                window.invalidate();
            }
            return;
        }
        let (x, y) = match self.pane_state(pane.pane_id()).mouse_terminal_coords {
            Some(coords) => (coords.0.column, coords.1),
            None => return,
        };
        self.select_text_at_coordinate(mode, pane, expected, x, y);
    }

    pub fn retry_pending_selection_start(&mut self, pane: &Arc<dyn Pane>) {
        let Some(pending) = self.pane_state(pane.pane_id()).pending_selection_start else {
            return;
        };
        if !pending.released
            && (self.active_selection_drag_pane != Some(pane.pane_id())
                || self.active_selection_drag_button != Some(pending.button)
                || !self.current_mouse_buttons.contains(&pending.button))
        {
            self.pane_state(pane.pane_id()).pending_selection_start = None;
            return;
        }
        let current = self.selection_frame_stamp(pane);
        let resolved = pending.resolve(current, &self.pane_state(pane.pane_id()).selection_frame);
        match resolved {
            crate::selection::PendingSelectionResolution::Ready => {}
            crate::selection::PendingSelectionResolution::Wait => return,
            crate::selection::PendingSelectionResolution::Invalidated => {
                self.clear_selection(pane);
                return;
            }
        };
        self.pane_state(pane.pane_id()).pending_selection_start = None;
        let SelectionX::Cell(x) = pending.coordinate.x else {
            return;
        };
        self.select_text_at_coordinate(
            pending.mode,
            pane,
            Some(pending.frame.authority),
            x,
            pending.coordinate.y,
        );
        // The retained endpoint belongs to this exact displayed gesture;
        // later motion after release must not change a deferred copy.
        if let Some((position, row)) = pending.end {
            if (position.column != x || row != pending.coordinate.y)
                && !self.extend_selection_at_position(
                    pending.mode,
                    pane,
                    Some((pending.frame.authority, position, row)),
                )
            {
                if !self
                    .selection(pane.pane_id())
                    .is_invalidated_by(SelectionAuthority::capture(&**pane))
                {
                    self.pane_state(pane.pane_id()).pending_selection_start = Some(pending);
                }
                return;
            }
        }
        if pane
            .downcast_ref::<frankenterm_client::pane::ClientPane>()
            .is_some()
        {
            // Copy starts at the current coherent source, but selected-row
            // mutation is judged against the frame where the gesture began.
            self.selection(pane.pane_id()).seqno = pending.frame.source_sequence;
        }
        if let Some(destination) = pending.copy {
            self.defer_pending_selection_copy(pane, destination);
        }
    }

    fn select_text_at_coordinate(
        &mut self,
        mode: SelectionMode,
        pane: &Arc<dyn Pane>,
        expected: Option<SelectionAuthority>,
        x: usize,
        y: StableRowIndex,
    ) {
        let mut desired = Selection::default();
        match mode {
            SelectionMode::Line => {
                let start = SelectionCoordinate::x_y(x, y);
                let (selection_range, pick) = SelectionRange::smart_or_line_around(start, &**pane);

                desired.origin = Some(start);
                desired.range = Some(selection_range);
                desired.rectangular = false;
                announce_pick_if_smart(pick);
            }
            SelectionMode::Word => {
                let (selection_range, pick) =
                    SelectionRange::smart_or_word_around(SelectionCoordinate::x_y(x, y), &**pane);

                desired.origin = Some(selection_range.start);
                desired.range = Some(selection_range);
                desired.rectangular = false;
                announce_pick_if_smart(pick);
            }
            SelectionMode::SemanticZone => {
                let selection_range =
                    SelectionRange::zone_around(SelectionCoordinate::x_y(x, y), &**pane);

                desired.origin = Some(selection_range.start);
                desired.range = Some(selection_range);
                desired.rectangular = false;
            }
            SelectionMode::Cell | SelectionMode::Block => {
                desired.begin(SelectionCoordinate::x_y(x, y));
                desired.rectangular = mode == SelectionMode::Block;
            }
        }

        desired.seqno = pane.get_current_seqno();
        desired.authority = expected;
        if desired.is_invalidated_by(SelectionAuthority::capture(&**pane)) {
            self.clear_selection(pane);
            return;
        }
        self.commit_selection_candidate(pane, desired);
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }
}

pub(crate) fn selected_text_from_logical_lines(
    logical_lines: &[LogicalLine],
    sel: SelectionRange,
    rectangular: bool,
) -> String {
    selected_lines_from_logical_lines(logical_lines, sel, rectangular)
        .iter()
        .map(|line| line.as_str().into_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

fn selected_line_span(
    line: &Line,
    row: StableRowIndex,
    next: Option<StableRowIndex>,
    sel: SelectionRange,
    rectangular: bool,
) -> (Line, bool, bool) {
    let cols = sel.cols_for_row(row, rectangular);
    let ends_before = !rectangular
        && line.last_cell_was_wrapped()
        && next.is_some_and(|next| {
            row.checked_add(1) == Some(next) && sel.cols_for_row(next, false).is_empty()
        });
    let continues = !rectangular
        && !ends_before
        && cols.end >= line.len()
        && line.last_cell_was_wrapped()
        && next.is_some_and(|next| {
            row.checked_add(1) == Some(next) && sel.cols_for_row(next, false).start == 0
        });
    let mut span = line.columns_as_line(cols);
    if !continues {
        let seqno = span.current_seqno();
        span.set_last_cell_was_wrapped(false, seqno);
        span.prune_trailing_blanks(seqno);
    }
    (span, continues, ends_before)
}

fn selected_lines_from_logical_lines(
    logical_lines: &[LogicalLine],
    sel: SelectionRange,
    rectangular: bool,
) -> Vec<Line> {
    let sel = sel.normalize();
    let selected_rows = sel.rows();
    let mut rows = logical_lines
        .iter()
        .flat_map(|logical| {
            logical
                .physical_lines
                .iter()
                .enumerate()
                .filter_map(move |(index, line)| {
                    let row = logical
                        .first_row
                        .checked_add(StableRowIndex::try_from(index).ok()?)?;
                    Some((row, line))
                })
        })
        .filter(|(row, _)| selected_rows.contains(row))
        .peekable();
    let mut result: Vec<Line> = Vec::new();
    let mut join_previous = false;
    while let Some((row, line)) = rows.next() {
        // BeforeZero on the next soft-wrapped row selects no cells there.
        // It is an endpoint, not a continuation that makes trailing blanks
        // significant. A hard line break still belongs to the selection.
        // Container boundaries may be synthetic budget cuts. Only the actual
        // selected contiguous wrapped cells determine continuation. Rectangles
        // remain separate physical rows even across terminal soft wraps.
        let (span, continues, ends_before_wrapped_row) = selected_line_span(
            line,
            row,
            rows.peek().map(|(next, _)| *next),
            sel,
            rectangular,
        );
        let seqno = span.current_seqno();
        if join_previous {
            if let Some(previous) = result.last_mut() {
                previous.append_line(span, seqno);
            }
        } else {
            result.push(span);
        }
        join_previous = continues;
        if ends_before_wrapped_row {
            break;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smart_selection_a11y::shared_smart_selection_recorder;
    use frankenterm_core::a11y_tree::{AccessibilityEvent, AnnouncePriority};
    use frankenterm_core::smart_selection::SelectionPatternKind;
    use proptest::prelude::*;
    use termwiz::cell::{CellAttributes, unicode_column_width};
    use termwiz::surface::SEQ_ZERO;

    #[test]
    fn local_selection_read_retires_busy_and_cancelled_workers_and_preserves_source_fence() {
        #[derive(Debug)]
        struct ReadConfig;
        impl wezterm_term::TerminalConfiguration for ReadConfig {
            fn color_palette(&self) -> wezterm_term::color::ColorPalette {
                Default::default()
            }
        }
        fn available_permit() -> mux::pane::LineReadPermit {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                if let Some(permit) = mux::pane::LineReadPermit::try_acquire() {
                    return permit;
                }
                assert!(std::time::Instant::now() < deadline, "read permit leaked");
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        let _other_readers: Vec<_> = (0..3).map(|_| available_permit()).collect();
        let mut terminal = wezterm_term::Terminal::new(
            wezterm_term::TerminalSize {
                rows: 2,
                cols: 40,
                dpi: 96,
                pixel_width: 320,
                pixel_height: 32,
            },
            Arc::new(ReadConfig),
            "selection-read-test",
            "test",
            Box::new(Vec::<u8>::new()),
        );
        terminal.advance_bytes("café 界 e\u{301}".as_bytes());
        let terminal = parking_lot::Mutex::new(terminal);
        let capture = || {
            Some(
                terminal
                    .try_lock()
                    .ok_or_else(|| anyhow::anyhow!("terminal busy"))
                    .and_then(|term| {
                        term.screen()
                            .capture_line_read_with_budget(0..1, &mut Default::default())
                    }),
            )
        };
        let deadline = || std::time::Instant::now() + std::time::Duration::from_secs(5);

        // A real held terminal lock abandons the already-started worker.
        let held = terminal.lock();
        assert!(
            LocalSelectionRead::start(capture, deadline(), || {})
                .unwrap()
                .is_none()
        );
        drop(held);
        drop(available_permit());

        // Completion remains charged while queued, and cancelling a queued
        // result returns its heavy payload to the worker before permit release.
        let (woke, wake) = sync_channel(1);
        let read = LocalSelectionRead::start(capture, deadline(), move || {
            woke.send(()).unwrap();
        })
        .unwrap()
        .unwrap();
        wake.recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(mux::pane::LineReadPermit::try_acquire().is_none());
        let cancelled = Arc::clone(&read.cancelled);
        let mut selection = Selection::default();
        selection.range = Some(SelectionRange::start(SelectionCoordinate::x_y(0, 0)));
        let mut copy = SelectionCopy::new(&selection, 12).unwrap();
        let copy_deadline = copy.deadline();
        copy.local_read = Some(read);
        let mut pending = Some(crate::selection::PendingNativeSelection::new(selection));
        pending.as_mut().unwrap().text_copy = Some(copy);
        // Exercise deadline retirement without any renderer or successful paint.
        assert!(expire_selection_copy_deadline(
            &mut pending,
            copy_deadline,
            copy_deadline,
        ));
        assert!(pending.is_none());
        assert!(cancelled.load(Ordering::Acquire));
        drop(available_permit());

        let read = LocalSelectionRead::start(capture, deadline(), || {})
            .unwrap()
            .unwrap();
        let ready = read
            .receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let plans = ready.plans.as_ref().unwrap().as_ref().unwrap();
        assert_eq!(plans.len(), 1);
        assert!(
            plans[0]
                .lines()
                .next()
                .unwrap()
                .as_str()
                .contains("café 界 e\u{301}")
        );
        assert!(terminal.lock().screen().validates_line_read(&plans[0]));
        terminal.lock().advance_bytes(b"\rCHANGED");
        assert!(!terminal.lock().screen().validates_line_read(&plans[0]));
        drop(ready);
        drop(read);
        drop(available_permit());
    }

    #[test]
    fn local_selection_copy_completes_a_valid_empty_span() {
        let mut selection = Selection::default();
        selection.range = Some(SelectionRange::start(SelectionCoordinate::x_y(0, 0)));
        let mut copy = SelectionCopy::new(&selection, 12).unwrap();
        copy.local = true;
        copy.push_chunk(vec![Line::from("")]).unwrap();
        assert_eq!(copy.finish(12).unwrap(), Some(String::new()));
    }

    #[test]
    fn selection_copy_deadline_retires_hidden_read_without_presentation() {
        let mut selection = Selection::default();
        selection.range = Some(SelectionRange::start(SelectionCoordinate::x_y(0, 0)));
        let mut copy = SelectionCopy::new(&selection, 12).unwrap();
        let deadline = copy.deadline();
        let cancelled = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = sync_channel(1);
        let (retire, retired) = sync_channel(1);
        sender
            .send(LocalSelectionReadReady {
                plans: Some(Ok(Vec::new())),
                retire,
            })
            .unwrap_or_else(|_| panic!("read receiver must be alive"));
        copy.local_read = Some(LocalSelectionRead {
            receiver,
            ready: None,
            cancelled: Arc::clone(&cancelled),
        });
        let (abort, registration) = futures::future::AbortHandle::new_pair();
        copy.deadline_wake = Some(SelectionCopyDeadline(abort.clone()));
        let mut pending = Some(crate::selection::PendingNativeSelection::new(selection));
        pending.as_mut().unwrap().text_copy = Some(copy);

        // An old notification must not retire a newer transaction, even if
        // dispatch was delayed beyond both deadlines.
        assert!(!expire_selection_copy_deadline(
            &mut pending,
            deadline - std::time::Duration::from_secs(1),
            deadline,
        ));
        assert!(!cancelled.load(Ordering::Acquire));
        assert!(!expire_selection_copy_deadline(
            &mut pending,
            deadline,
            deadline - std::time::Duration::from_nanos(1),
        ));
        assert!(expire_selection_copy_deadline(
            &mut pending,
            deadline,
            deadline,
        ));
        assert!(pending.is_none());
        assert!(cancelled.load(Ordering::Acquire));
        assert!(abort.is_aborted());
        assert!(retired.try_recv().unwrap().unwrap().is_empty());
        drop(registration);
    }

    #[test]
    fn remote_selection_copy_expiry_releases_hidden_pane_text_once() {
        let mut selection = Selection::default();
        selection.range = Some(SelectionRange {
            start: SelectionCoordinate::x_y(0, 0),
            end: SelectionCoordinate::x_y(3, 2),
        });
        let mut copy = SelectionCopy::new(&selection, 12).unwrap();
        copy.push_chunk(vec![Line::from("retained"), Line::from("text")])
            .unwrap();
        assert!(!copy.text.is_empty());
        let deadline = copy.deadline();
        let mut intent = crate::selection::PendingNativeSelection::new(selection);
        intent.copy = Some(config::keyassignment::ClipboardCopyDestination::Clipboard);
        intent.text_copy = Some(copy);
        let mut hidden = Some(intent);
        assert!(!crate::selection::PendingNativeSelection::expire_text_copy(
            &mut hidden,
            deadline - std::time::Duration::from_nanos(1)
        ));
        assert!(hidden.is_some());
        assert!(crate::selection::PendingNativeSelection::expire_text_copy(
            &mut hidden,
            deadline
        ));
        assert!(
            hidden.is_none(),
            "hidden pane retains neither text nor clipboard intent after expiry"
        );
        assert!(!crate::selection::PendingNativeSelection::expire_text_copy(
            &mut hidden,
            deadline
        ));
    }

    #[test]
    fn remote_selection_copy_never_publishes_partial_or_changed_source_text() {
        let mut selection = Selection::default();
        selection.range = Some(SelectionRange {
            start: SelectionCoordinate::x_y(0, 0),
            end: SelectionCoordinate::x_y(3, 1),
        });
        let mut copy = SelectionCopy::new(&selection, 12).unwrap();
        copy.push_chunk(vec![Line::from("界 e\u{301}")]).unwrap();
        assert_eq!(copy.finish(12).unwrap(), None);
        assert!(copy.finish(13).is_err());
        copy.push_chunk(vec![Line::from("tail")]).unwrap();
        assert!(
            copy.finish(13).is_err(),
            "completion cannot publish after the source changed"
        );
        assert_eq!(
            copy.finish(12).unwrap(),
            Some("界 e\u{301}\ntail".to_string())
        );
        let mut expired = SelectionCopy::new(&selection, 12).unwrap();
        expired.deadline = std::time::Instant::now();
        assert!(expired.finish(12).is_err());
    }

    #[test]
    fn remote_selection_copy_chunks_preserve_unicode_wrap_and_endpoint_semantics() {
        for rectangular in [false, true] {
            for before_zero in [false, true] {
                let mut physical = (0..66)
                    .map(|_| Line::from_text("", &CellAttributes::default(), 7, None))
                    .collect::<Vec<_>>();
                physical[63] = Line::from_text("界 e\u{301} ", &CellAttributes::default(), 7, None);
                physical[63].set_last_cell_was_wrapped(true, 7);
                physical[64] = Line::from_text("tail ", &CellAttributes::default(), 7, None);
                physical[65] = Line::from_text("last", &CellAttributes::default(), 7, None);
                let range = SelectionRange {
                    start: SelectionCoordinate::x_y(0, 0),
                    end: SelectionCoordinate {
                        x: if before_zero {
                            SelectionX::BeforeZero
                        } else {
                            SelectionX::Cell(3)
                        },
                        y: if before_zero { 64 } else { 65 },
                    },
                };
                let mut selection = Selection::default();
                selection.range = Some(range);
                selection.rectangular = rectangular;
                let expected = selected_text_from_logical_lines(
                    &[logical_line_from_physical(physical.clone())],
                    range,
                    rectangular,
                );
                if !rectangular {
                    assert_eq!(
                        expected,
                        format!(
                            "{}{}",
                            "\n".repeat(63),
                            if before_zero {
                                "界 e\u{301}"
                            } else {
                                "界 e\u{301} tail\nlast"
                            }
                        )
                    );
                }
                for chunk_size in [1, 64] {
                    let mut copy = SelectionCopy::new(&selection, 9).unwrap();
                    let end = usize::try_from(copy.end_row).unwrap();
                    let deadline = copy.deadline;
                    for chunk in physical[..end].chunks(chunk_size) {
                        copy.push_chunk(chunk.to_vec()).unwrap();
                        assert_eq!(copy.deadline, deadline);
                    }
                    assert_eq!(copy.next_row, copy.end_row);
                    assert_eq!(copy.text, expected);
                }
            }
        }
    }

    #[test]
    fn native_selection_before_zero_endpoint_trims_soft_wrap_tail_but_preserves_hard_newline() {
        for wrapped in [false, true] {
            let mut first = Line::from_text("A ", &CellAttributes::default(), SEQ_ZERO, None);
            first.set_last_cell_was_wrapped(wrapped, SEQ_ZERO);
            let next = Line::from_text("B", &CellAttributes::default(), SEQ_ZERO, None);
            let lines = vec![logical_line_from_physical(vec![first, next])];
            for reverse in [false, true] {
                let start = SelectionCoordinate::x_y(0, 0);
                let end = SelectionCoordinate {
                    x: SelectionX::BeforeZero,
                    y: 1,
                };
                let selection = if reverse {
                    SelectionRange {
                        start: end,
                        end: start,
                    }
                } else {
                    SelectionRange { start, end }
                };
                assert_eq!(
                    selected_text_from_logical_lines(&lines, selection, false),
                    if wrapped { "A" } else { "A\n" }
                );
            }
        }
        let lines = vec![logical_line_from_physical(vec![
            Line::from_text("A", &CellAttributes::default(), SEQ_ZERO, None),
            Line::from_text("", &CellAttributes::default(), SEQ_ZERO, None),
            Line::from_text("B", &CellAttributes::default(), SEQ_ZERO, None),
        ])];
        assert_eq!(
            selected_text_from_logical_lines(
                &lines,
                SelectionRange::start(SelectionCoordinate::x_y(0, 0))
                    .extend(SelectionCoordinate::x_y(0, 2)),
                false,
            ),
            "A\n\nB"
        );
    }

    #[test]
    fn bounded_logical_groups_preserve_selected_wrapped_spaces_in_text_and_lines() {
        let prefix = format!("{}  ", "a".repeat(mux::pane::MAX_LOGICAL_LINE_LEN - 2));
        let mut first = Line::from_text(&prefix, &CellAttributes::default(), SEQ_ZERO, None);
        first.set_last_cell_was_wrapped(true, SEQ_ZERO);
        let second = Line::from_text("z", &CellAttributes::default(), SEQ_ZERO, None);
        let mut tail = logical_line_from_physical(vec![second]);
        tail.first_row = 1;
        let groups = vec![logical_line_from_physical(vec![first]), tail];
        let selection = SelectionRange::start(SelectionCoordinate::x_y(0, 0))
            .extend(SelectionCoordinate::x_y(0, 1));
        let expected = format!("{prefix}z");
        assert_eq!(
            selected_text_from_logical_lines(&groups, selection, false),
            expected
        );
        let rich = selected_lines_from_logical_lines(&groups, selection, false);
        assert_eq!(rich.len(), 1);
        assert_eq!(rich[0].as_str(), expected);
        assert_eq!(
            selected_text_from_logical_lines(&groups, selection, true),
            "a\nz"
        );
        // A gap is not a wrapped continuation and must not concatenate rows.
        let mut gap = groups;
        gap[1].first_row = 2;
        let selection = SelectionRange::start(SelectionCoordinate::x_y(0, 0))
            .extend(SelectionCoordinate::x_y(0, 2));
        assert_eq!(
            selected_text_from_logical_lines(&gap, selection, false),
            format!("{}\nz", "a".repeat(mux::pane::MAX_LOGICAL_LINE_LEN - 2))
        );
    }

    fn logical_line_from_physical(physical_lines: Vec<Line>) -> LogicalLine {
        let logical_text = physical_lines
            .iter()
            .map(Line::as_str)
            .collect::<Vec<_>>()
            .join("");
        LogicalLine {
            physical_lines,
            logical: Line::from_text(&logical_text, &CellAttributes::default(), SEQ_ZERO, None),
            first_row: 0,
        }
    }

    fn arb_selection_glyph() -> impl Strategy<Value = &'static str> {
        prop_oneof![
            Just("A"),
            Just("z"),
            Just("0"),
            Just("-"),
            Just("\u{00e9}"),
            Just("e\u{0301}"),
            Just("a\u{0308}"),
            Just("\u{03bb}"),
            Just("\u{4e2d}"),
            Just("\u{754c}"),
            Just("\u{8a9e}"),
            Just("\u{1f480}"),
            Just("\u{1f9ea}"),
        ]
    }

    fn arb_selection_payload() -> impl Strategy<Value = String> {
        proptest::collection::vec(arb_selection_glyph(), 1..32).prop_map(|glyphs| glyphs.concat())
    }

    fn arb_wrapped_selection_payload() -> impl Strategy<Value = (String, String)> {
        proptest::collection::vec(arb_selection_glyph(), 2..32)
            .prop_flat_map(|glyphs| {
                let split_range = 1..glyphs.len();
                (Just(glyphs), split_range)
            })
            .prop_map(|(glyphs, split)| {
                let head = glyphs[..split].concat();
                let tail = glyphs[split..].concat();
                (head, tail)
            })
    }

    fn arb_double_width_anchor() -> impl Strategy<Value = (String, &'static str, String)> {
        (
            proptest::collection::vec(arb_selection_glyph(), 0..12),
            prop_oneof![
                Just("\u{4e2d}"),
                Just("\u{754c}"),
                Just("\u{8a9e}"),
                Just("\u{1f480}"),
                Just("\u{1f9ea}"),
            ],
            proptest::collection::vec(arb_selection_glyph(), 0..12),
        )
            .prop_map(|(prefix, wide, suffix)| (prefix.concat(), wide, suffix.concat()))
    }

    fn arb_selection_glyphs() -> impl Strategy<Value = Vec<&'static str>> {
        proptest::collection::vec(arb_selection_glyph(), 1..32)
    }

    fn selected_text_for_range(line: Line, start_col: usize, end_col: usize) -> String {
        let selected = SelectionRange::start(SelectionCoordinate::x_y(start_col, 0))
            .extend(SelectionCoordinate::x_y(end_col, 0));
        selected_text_from_logical_lines(&[logical_line_from_physical(vec![line])], selected, false)
    }

    fn wrapped_logical_line_from_glyphs(
        glyphs: &[&'static str],
        width: usize,
    ) -> (LogicalLine, Vec<(StableRowIndex, usize, usize)>) {
        let attrs = CellAttributes::default();
        let mut physical_lines = Vec::new();
        let mut mappings = Vec::with_capacity(glyphs.len());
        let mut current_text = String::new();
        let mut current_col = 0usize;
        let mut current_row = 0isize;

        for glyph in glyphs {
            let glyph_width = unicode_column_width(glyph, None).max(1);
            if current_col > 0 && current_col + glyph_width > width {
                physical_lines.push(Line::from_text(&current_text, &attrs, SEQ_ZERO, None));
                current_text.clear();
                current_col = 0;
                current_row += 1;
            }

            mappings.push((current_row, current_col, glyph_width));
            current_text.push_str(glyph);
            current_col += glyph_width;
        }

        physical_lines.push(Line::from_text(&current_text, &attrs, SEQ_ZERO, None));
        let last_idx = physical_lines.len().saturating_sub(1);
        for line in physical_lines.iter_mut().take(last_idx) {
            line.set_last_cell_was_wrapped(true, SEQ_ZERO);
        }

        (logical_line_from_physical(physical_lines), mappings)
    }

    fn select_glyph_span_after_wrapping(
        glyphs: &[&'static str],
        width: usize,
        start_idx: usize,
        end_idx: usize,
    ) -> String {
        let (logical, mappings) = wrapped_logical_line_from_glyphs(glyphs, width);
        let (start_row, start_col, _) = mappings[start_idx];
        let (end_row, end_col, end_width) = mappings[end_idx];
        let selected =
            SelectionRange::start(SelectionCoordinate::x_y(start_col, start_row)).extend(
                SelectionCoordinate::x_y(end_col + end_width.saturating_sub(1), end_row),
            );

        selected_text_from_logical_lines(&[logical], selected, false)
    }

    #[test]
    fn selection_clipboard_text_preserves_wide_and_combining_glyphs() {
        let payload = "A界e\u{0301}\u{1f480}Z";
        let line = Line::from_text(payload, &CellAttributes::default(), SEQ_ZERO, None);
        assert!(
            line.len() > payload.chars().count(),
            "fixture must include at least one multi-column glyph"
        );
        let selected = SelectionRange::start(SelectionCoordinate::x_y(0, 0))
            .extend(SelectionCoordinate::x_y(line.len().saturating_sub(1), 0));

        let text = selected_text_from_logical_lines(
            &[logical_line_from_physical(vec![line])],
            selected,
            false,
        );

        assert_eq!(text, payload);
    }

    #[test]
    fn selection_clipboard_text_preserves_unicode_across_wrapped_rows() {
        let attrs = CellAttributes::default();
        let mut wrapped = Line::from_text_with_wrapped_last_col("A界", &attrs, SEQ_ZERO);
        let tail_payload = "e\u{0301}\u{1f480}Z";
        let tail = Line::from_text(tail_payload, &attrs, SEQ_ZERO, None);
        let selected = SelectionRange::start(SelectionCoordinate::x_y(0, 0))
            .extend(SelectionCoordinate::x_y(tail.len().saturating_sub(1), 1));
        wrapped.set_last_cell_was_wrapped(true, SEQ_ZERO);

        let text = selected_text_from_logical_lines(
            &[logical_line_from_physical(vec![wrapped, tail])],
            selected,
            false,
        );

        assert_eq!(text, format!("A界{tail_payload}"));
    }

    #[test]
    fn selection_clipboard_text_ignores_logical_lines_with_no_physical_rows() {
        let attrs = CellAttributes::default();
        let first = Line::from_text("first", &attrs, SEQ_ZERO, None);
        let second = Line::from_text("second", &attrs, SEQ_ZERO, None);
        let empty = LogicalLine {
            physical_lines: vec![],
            logical: Line::new(SEQ_ZERO),
            first_row: 1,
        };
        let second_len = second.len();
        let mut first_logical = logical_line_from_physical(vec![first]);
        first_logical.first_row = 0;
        let mut second_logical = logical_line_from_physical(vec![second]);
        second_logical.first_row = 2;
        let selected = SelectionRange::start(SelectionCoordinate::x_y(0, 0))
            .extend(SelectionCoordinate::x_y(second_len.saturating_sub(1), 2));

        let text = selected_text_from_logical_lines(
            &[first_logical, empty, second_logical],
            selected,
            false,
        );

        assert_eq!(text, "first\nsecond");
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]

        #[test]
        fn selection_clipboard_roundtrip_preserves_generated_unicode_glyphs(
            payload in arb_selection_payload()
        ) {
            let attrs = CellAttributes::default();
            let line = Line::from_text(&payload, &attrs, SEQ_ZERO, None);
            let first_copy = selected_text_for_range(line, 0, payload.len().max(1));
            let copied_line = Line::from_text(&first_copy, &attrs, SEQ_ZERO, None);
            let second_copy = selected_text_for_range(copied_line, 0, first_copy.len().max(1));

            prop_assert_eq!(&first_copy, &payload);
            prop_assert_eq!(&second_copy, &payload);
        }

        #[test]
        fn wrapped_selection_clipboard_roundtrip_preserves_generated_unicode_glyphs(
            (head, tail) in arb_wrapped_selection_payload()
        ) {
            let attrs = CellAttributes::default();
            let mut wrapped = Line::from_text_with_wrapped_last_col(&head, &attrs, SEQ_ZERO);
            let tail_line = Line::from_text(&tail, &attrs, SEQ_ZERO, None);
            let selected = SelectionRange::start(SelectionCoordinate::x_y(0, 0))
                .extend(SelectionCoordinate::x_y(tail_line.len().saturating_sub(1), 1));
            wrapped.set_last_cell_was_wrapped(true, SEQ_ZERO);

            let copied = selected_text_from_logical_lines(
                &[logical_line_from_physical(vec![wrapped, tail_line])],
                selected,
                false,
            );
            let expected = format!("{head}{tail}");
            let copied_line = Line::from_text(&copied, &attrs, SEQ_ZERO, None);
            let recopied = selected_text_for_range(copied_line, 0, copied.len().max(1));

            prop_assert_eq!(&copied, &expected);
            prop_assert_eq!(&recopied, &expected);
        }

        #[test]
        fn selection_clipboard_double_width_boundaries_never_emit_half_glyphs(
            (prefix, wide, suffix) in arb_double_width_anchor()
        ) {
            let attrs = CellAttributes::default();
            let payload = format!("{prefix}{wide}{suffix}");
            let line = Line::from_text(&payload, &attrs, SEQ_ZERO, None);
            let wide_start = unicode_column_width(&prefix, None);
            let wide_width = unicode_column_width(wide, None);
            prop_assert_eq!(wide_width, 2, "fixture must generate double-width anchors");

            let selected_wide =
                selected_text_for_range(line.clone(), wide_start, wide_start + wide_width - 1);
            let selected_from_inside =
                selected_text_for_range(line, wide_start + 1, wide_start + wide_width - 1);

            prop_assert_eq!(selected_wide, wide);
            prop_assert!(
                selected_from_inside.is_empty(),
                "selection starting inside a double-width glyph must not emit a partial glyph"
            );
        }

        #[test]
        fn selection_text_is_stable_when_same_logical_span_is_rewrapped_by_resize(
            glyphs in arb_selection_glyphs(),
            first_width in 2usize..=16,
            second_width in 2usize..=16,
        ) {
            let last_idx = glyphs.len() - 1;
            let start_idx = last_idx / 3;
            let end_idx = (start_idx + (glyphs.len().max(2) / 2)).min(last_idx);
            let expected = glyphs[start_idx..=end_idx].concat();

            let first = select_glyph_span_after_wrapping(&glyphs, first_width, start_idx, end_idx);
            let second = select_glyph_span_after_wrapping(&glyphs, second_width, start_idx, end_idx);

            prop_assert_eq!(
                &first,
                &expected,
                "selection changed while projecting logical span into first resized width={} glyphs={:?}",
                first_width,
                glyphs
            );
            prop_assert_eq!(
                &second,
                &expected,
                "selection changed while projecting logical span into second resized width={} glyphs={:?}",
                second_width,
                glyphs
            );
            prop_assert_eq!(
                &first,
                &second,
                "selection text must survive rewrap from width {} to {} for glyphs={:?}",
                first_width,
                second_width,
                glyphs
            );
        }
    }

    #[test]
    fn announce_pick_if_smart_emits_mouse_selection_announcement() {
        let _guard = crate::smart_selection_a11y::tests::shared_recorder_test_lock();
        let sentinel = "https://example.com/gui-mouse-selection-sentinel";
        let _ = shared_smart_selection_recorder().take();

        announce_pick_if_smart(Some(SmartSelectionPick {
            kind: SelectionPatternKind::Url,
            text: sentinel.to_string(),
        }));

        let event = shared_smart_selection_recorder()
            .find_announcement_for_kind(SelectionPatternKind::Url)
            .expect("URL announcement from GUI mouse selection bridge");

        match event {
            AccessibilityEvent::AnnounceMessage {
                value, priority, ..
            } => {
                assert_eq!(value, format!("URL selected: {sentinel}"));
                assert_eq!(priority, AnnouncePriority::Polite);
            }
            other => panic!("expected AnnounceMessage, got {other:?}"),
        }

        let _ = shared_smart_selection_recorder().take();
    }
}
