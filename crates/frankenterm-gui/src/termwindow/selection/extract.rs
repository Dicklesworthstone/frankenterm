//! Selection text extraction on a worker thread (ft-yccm0.2.2.4).
//!
//! Copying a local pane's selection used to advance from the paint path,
//! one 64-row chunk per frame, each step a `try_lock` of the terminal that a
//! flooding parser rarely leaves free, so a large or busy copy could run out
//! its deadline. A worker now reads the selection in slices of
//! [`SLICE_ROWS`] rows. For each slice it proves the selection still selects
//! the same rows and captures them under brief terminal locks that register
//! demand (a flooding parser hands the lock over at its next slice),
//! hydrates cold rows with no lock held, publishes and clones them under one
//! more brief lock, and appends their text. These are the per-frame path's
//! steps and checks. The text goes back to the UI thread in a
//! `TermWindowNotif::Apply`, which copies it if the same selection is still
//! pending; dropping the pending copy (a new selection, the deadline)
//! cancels the worker. `FT_DISABLE_SELECTION_EXTRACTION_WORKER=1` keeps the
//! per-frame path for the native A/B.

use super::SelectionCopy;
use crate::selection::{PendingNativeSelection, Selection, SelectionAuthority};
use config::keyassignment::ClipboardCopyDestination;
use mux::localpane::LocalPane;
use mux::pane::{LineReadPermit, Pane};
use mux::renderable::RenderableDimensions;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use termwiz::surface::SequenceNo;
use wezterm_term::StableRowIndex;
use wezterm_term::screen::ScreenLineRead;

/// Rows per slice: the most one terminal-lock hold covers.
pub(super) const SLICE_ROWS: StableRowIndex = 512;
/// The longest the worker waits for the terminal before it checks for
/// cancellation and the deadline again.
const LOCK_WAIT: Duration = Duration::from_millis(25);
/// The pause before retrying a busy terminal, reader pool or cold source.
const RETRY_PAUSE: Duration = Duration::from_millis(2);

const STOPPED: &str = "The selected text did not arrive in time. Copy the selection again.";

/// Same-binary control for the native A/B.
pub(super) fn worker_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var_os("FT_DISABLE_SELECTION_EXTRACTION_WORKER").as_deref()
            != Some(std::ffi::OsStr::new("1"))
    })
}

pub(super) type Outcome = Result<String, &'static str>;

/// A running extraction. Dropping it cancels the worker.
pub(crate) struct SelectionExtraction {
    generation: u64,
    cancelled: Arc<AtomicBool>,
    outcome: Arc<Mutex<Option<Outcome>>>,
}

impl std::fmt::Debug for SelectionExtraction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SelectionExtraction")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl Drop for SelectionExtraction {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

impl SelectionExtraction {
    /// Starts reading `desired`'s text from `pane`, a [`LocalPane`], into
    /// `copy`, under `copy`'s deadline. Unless cancelled first, the worker
    /// stores its outcome and then calls `done` with this extraction's
    /// generation.
    pub(super) fn start(
        pane: Weak<dyn Pane>,
        desired: Selection,
        copy: SelectionCopy,
        done: impl FnOnce(u64) + Send + 'static,
    ) -> Result<Self, &'static str> {
        static GENERATION: AtomicU64 = AtomicU64::new(1);
        let generation = GENERATION.fetch_add(1, Ordering::Relaxed);
        let cancelled = Arc::new(AtomicBool::new(false));
        let outcome = Arc::new(Mutex::new(None));
        let (worker_cancelled, worker_outcome) = (Arc::clone(&cancelled), Arc::clone(&outcome));
        std::thread::Builder::new()
            .name("selection-extract".into())
            .spawn(move || {
                let mut job = Job {
                    desired,
                    copy,
                    slice_rows: SLICE_ROWS,
                };
                let result = job.run(&pane, &worker_cancelled);
                if worker_cancelled.load(Ordering::Acquire) {
                    return;
                }
                *worker_outcome
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(result);
                done(generation);
            })
            .map_err(|_| "The text reader could not start. Copy the selection again.")?;
        Ok(Self {
            generation,
            cancelled,
            outcome,
        })
    }

    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    /// The text, or why there is none, once the worker has finished.
    pub(super) fn take_outcome(&self) -> Option<Outcome> {
        self.outcome
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }
}

/// Whether `current`, the pane's selection now, is the one `expected` was
/// copied from: the same native anchor, or without one the same selection.
pub(super) fn same_copied_selection(expected: &Selection, current: &Selection) -> bool {
    match (expected.native_anchor(), current.native_anchor()) {
        (Some(expected), Some(current)) => expected == current,
        (None, None) => expected == current,
        _ => false,
    }
}

/// Takes the finished outcome of extraction `generation` out of `pending`,
/// with its clipboard destination. Nothing is taken from a pending copy that
/// another extraction serves, or that is still running. A finished copy is
/// retired either way, and its text is dropped unless `current` is still
/// the selection it was copied from.
pub(super) fn take_completed_copy(
    pending: &mut Option<PendingNativeSelection>,
    current: &Selection,
    generation: u64,
) -> Option<(Outcome, Option<ClipboardCopyDestination>)> {
    let copy = pending.as_ref()?;
    let extraction = copy.text_copy.as_ref()?.extraction.as_ref()?;
    if extraction.generation() != generation {
        return None;
    }
    let outcome = extraction.take_outcome()?;
    let copy = pending.take()?;
    same_copied_selection(&copy.desired, current).then_some((outcome, copy.copy))
}

/// One extraction's state on its worker.
struct Job {
    desired: Selection,
    copy: SelectionCopy,
    slice_rows: StableRowIndex,
}

/// A captured, hydrated slice waiting to be published. It keeps its reader
/// permit until the slice is read or abandoned.
struct SliceRead {
    rows: Range<StableRowIndex>,
    read: ScreenLineRead,
    _permit: LineReadPermit,
}

impl Job {
    fn run(&mut self, pane: &Weak<dyn Pane>, cancelled: &AtomicBool) -> Outcome {
        let deadline = self.copy.deadline();
        let stopped = || cancelled.load(Ordering::Acquire) || Instant::now() >= deadline;
        let mut slice: Option<SliceRead> = None;
        let mut proven = false;
        loop {
            if stopped() {
                return Err(STOPPED);
            }
            let pane = pane.upgrade().ok_or("The pane closed while copying.")?;
            let local = pane
                .downcast_ref::<LocalPane>()
                .ok_or("This pane cannot provide a bounded text read.")?;
            let Some((sequence, dimensions)) = self.refresh(&pane, local, &mut proven)? else {
                std::thread::sleep(RETRY_PAUSE);
                continue;
            };
            if self.copy.next_row >= self.copy.end_row {
                return self
                    .copy
                    .finish(sequence)?
                    .ok_or("The selected text is incomplete. Copy the selection again.");
            }
            let requested = self.copy.next_row
                ..self
                    .copy
                    .next_row
                    .saturating_add(self.slice_rows)
                    .min(self.copy.end_row);
            if slice.as_ref().is_none_or(|slice| slice.rows != requested) {
                slice = None;
                let Some(permit) = LineReadPermit::try_acquire() else {
                    std::thread::sleep(RETRY_PAUSE);
                    continue;
                };
                let read = match local.capture_line_read_waiting(
                    requested.clone(),
                    &mut Default::default(),
                    LOCK_WAIT,
                ) {
                    Some(Ok(read)) => read,
                    // Busy, or cold metadata not yet readable: retry, as the
                    // per-frame path does on its next frame.
                    Some(Err(_)) => {
                        std::thread::sleep(RETRY_PAUSE);
                        continue;
                    }
                    None => return Err("This pane cannot provide a bounded text read."),
                };
                // Cold rows hydrate here, with no terminal lock held.
                let read = read
                    .with_requested_physical_rows_only()
                    .hydrate_with_payload_limit(ScreenLineRead::MAX_PAYLOAD_BYTES, stopped)
                    .map_err(|_| {
                        if stopped() {
                            STOPPED
                        } else {
                            "The selected history could not be loaded. Copy the selection again."
                        }
                    })?;
                slice = Some(SliceRead {
                    rows: requested.clone(),
                    read,
                    _permit: permit,
                });
            }
            let read = &slice.as_ref().expect("captured above").read;
            let mut rows = None;
            let published = local
                .publish_line_reads_at_layout_waiting(
                    std::slice::from_ref(read),
                    sequence,
                    dimensions,
                    &mut || {
                        let mut bytes = ScreenLineRead::MAX_PAYLOAD_BYTES;
                        let mut work = 65_536;
                        rows = read.try_clone_viewport_for_snapshot(
                            requested.clone(),
                            &mut bytes,
                            &mut work,
                        );
                    },
                    LOCK_WAIT,
                )
                .unwrap_or(false);
            if !published {
                std::thread::sleep(RETRY_PAUSE);
                continue;
            }
            let (first, rows) = rows.ok_or("The selected rows exceed the text read budget.")?;
            if first != requested.start
                || usize::try_from(requested.end - requested.start).ok() != Some(rows.len())
            {
                return Err("The selected history changed or is unavailable. Select it again.");
            }
            // Retire the hydrated slice and its permit before the text work.
            slice = None;
            self.copy.push_chunk(rows)?;
        }
    }

    /// Proves the selection still selects the rows being copied, as of a
    /// fresh observation of the pane, as the per-frame path does before each
    /// chunk and at the end: its sequence and dimensions, or None while the
    /// pane is busy. A native anchor tolerates unrelated output; without one
    /// (a rectangular selection) any output since the copy began fails it.
    fn refresh(
        &mut self,
        pane: &Arc<dyn Pane>,
        local: &LocalPane,
        proven: &mut bool,
    ) -> Result<Option<(SequenceNo, RenderableDimensions)>, &'static str> {
        let authority = self
            .desired
            .authority
            .ok_or("The selection range is unavailable. Select the text again.")?;
        let observation = match self.desired.native_anchor() {
            Some(anchor) => {
                let Some((floor, observed, dimensions, points)) =
                    local.selection_anchor_snapshot_waiting(anchor, LOCK_WAIT)
                else {
                    return Ok(None);
                };
                if SelectionAuthority::from_native_snapshot(&**pane, floor, dimensions)
                    != Some(authority)
                {
                    return Err("The pane layout changed while copying. Select the text again.");
                }
                self.copy
                    .follow_unchanged_native_selection(&self.desired, observed, points)?;
                (observed, dimensions)
            }
            None => {
                let Some((floor, observed, dimensions)) =
                    local.selection_source_snapshot_waiting(LOCK_WAIT)
                else {
                    return Ok(None);
                };
                if SelectionAuthority::from_native_snapshot(&**pane, floor, dimensions)
                    != Some(authority)
                {
                    return Err("The pane changed while copying. Copy the selection again.");
                }
                if !*proven {
                    // The copy begins with this observation, as the per-frame
                    // path's begins with its first source capture.
                    self.copy.source_sequence = observed;
                }
                self.copy.verify_source(observed)?;
                (observed, dimensions)
            }
        };
        *proven = true;
        Ok(Some(observation))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::selection::{SelectionCoordinate, SelectionRange};
    #[cfg(unix)]
    use mux::pane::PaneId;

    fn job(desired: &Selection, slice_rows: StableRowIndex) -> Job {
        Job {
            desired: desired.clone(),
            copy: SelectionCopy::new(desired, desired.seqno).unwrap(),
            slice_rows,
        }
    }

    /// Kills a fixture pane's child when dropped.
    #[cfg(unix)]
    struct RetireChild(Arc<dyn Pane>);

    #[cfg(unix)]
    impl Drop for RetireChild {
        fn drop(&mut self) {
            self.0.kill();
        }
    }

    /// A local pane whose history spans the cold store, the warm tier and
    /// hot rows: wrapped lines, wide characters, combining marks and empty
    /// lines. Its whole history's layout is published, as paint publishes
    /// the rows it shows before they can be selected.
    #[cfg(unix)]
    struct Tiered {
        pane: Arc<dyn Pane>,
        /// The first row held in memory (warm, then hot); rows before it are
        /// in the cold store.
        resident_top: StableRowIndex,
        child: RetireChild,
        store: tempfile::TempDir,
    }

    #[cfg(unix)]
    fn tiered_pane() -> Tiered {
        use wezterm_term::config::{ScrollbackSpillSink, ScrollbackTierConfig};

        #[derive(Debug)]
        struct TierConfig(Arc<dyn ScrollbackSpillSink>);
        impl wezterm_term::TerminalConfiguration for TierConfig {
            fn color_palette(&self) -> wezterm_term::color::ColorPalette {
                Default::default()
            }
            fn scrollback_size(&self) -> usize {
                4096
            }
            fn scrollback_tier_config(&self) -> ScrollbackTierConfig {
                ScrollbackTierConfig {
                    enabled: true,
                    hot_lines: 24,
                    warm_max_bytes: 4096,
                }
            }
            fn scrollback_spill_sink(&self) -> Option<Arc<dyn ScrollbackSpillSink>> {
                Some(Arc::clone(&self.0))
            }
        }

        let root = tempfile::tempdir().unwrap();
        let sink = frankenterm_mux_server_impl::open_scrollback_spill_sink(
            root.path().to_path_buf(),
            &config::ScrollbackSpillSinkContext {
                pane_id: 998_311,
                domain_id: 998_311,
                durable_pane_id: *uuid::Uuid::new_v4().as_bytes(),
                command_description: "selection extraction parity".to_owned(),
            },
        )
        .unwrap();
        let mut terminal = wezterm_term::Terminal::new(
            wezterm_term::TerminalSize {
                rows: 4,
                cols: 24,
                dpi: 96,
                pixel_width: 192,
                pixel_height: 64,
            },
            Arc::new(TierConfig(Arc::clone(&sink))),
            "selection-extraction-test",
            "test",
            Box::new(Vec::<u8>::new()),
        );
        write_history(&mut terminal, || sink.flush_scrollback().unwrap());
        let status = terminal.screen().tiered_scrollback_status();
        assert!(
            status.cold_sink_retained_lines > 0
                && status.warm_resident_lines > 0
                && status.in_memory_scrollback_rows > status.warm_resident_lines,
            "the fixture spans cold, warm and hot rows: {status:?}"
        );
        let resident_top = terminal.screen().phys_to_stable_row_index(0);
        assert!(resident_top > 40, "{resident_top}");

        let (pane, guard) = local_pane(998_311, terminal);
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            assert!(
                Instant::now() < deadline,
                "the history layout never published"
            );
            let (_, sequence, dimensions) = SelectionAuthority::capture_source(&*pane).unwrap();
            let end = dimensions.physical_top + dimensions.viewport_rows as StableRowIndex;
            let read = pane
                .capture_line_read(0..end, &mut Default::default())
                .unwrap()
                .and_then(|read| read.hydrate(|| false));
            let Ok(read) = read else {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            };
            // A layout change is installed and reported as unpublished;
            // the next round publishes at the new layout.
            if pane
                .publish_line_reads_at_layout(&[read], sequence, dimensions, &mut || {})
                .unwrap_or(false)
            {
                break;
            }
        }
        Tiered {
            pane,
            resident_top,
            child: guard,
            store: root,
        }
    }

    /// 400 lines, then `END`: wrapped lines, wide characters, combining
    /// marks and empty lines. `flush` runs every 32 lines and at the end.
    #[cfg(unix)]
    fn write_history(terminal: &mut wezterm_term::Terminal, mut flush: impl FnMut()) {
        for index in 0..400 {
            let text = match index % 5 {
                0 => format!("{index:03} 界e\u{301}界e\u{301} wide and combining"),
                1 => format!("{index:03} {}", "long wrapped text ".repeat(4)),
                2 => format!("{index:03} short"),
                3 => String::new(),
                _ => format!("{index:03} tail 界"),
            };
            terminal.advance_bytes(text.as_bytes());
            terminal.advance_bytes(b"\r\n");
            if index % 32 == 31 {
                flush();
            }
        }
        terminal.advance_bytes("END 界e\u{301}".as_bytes());
        flush();
    }

    /// `terminal` as local pane `id`, with a `cat` child its guard kills.
    #[cfg(unix)]
    fn local_pane(id: PaneId, terminal: wezterm_term::Terminal) -> (Arc<dyn Pane>, RetireChild) {
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize::default())
            .unwrap();
        let writer = pair.master.take_writer().unwrap();
        let child = pair
            .slave
            .spawn_command(portable_pty::CommandBuilder::new("/bin/cat"))
            .unwrap();
        let pane: Arc<dyn Pane> = Arc::new(LocalPane::new(
            id,
            terminal,
            child,
            pair.master,
            writer,
            id,
            [0x34; 16],
            "selection extraction test".to_owned(),
        ));
        let guard = RetireChild(Arc::clone(&pane));
        (pane, guard)
    }

    /// The same history held entirely in memory, with no tiers: where a
    /// native anchor is captured at once.
    #[cfg(unix)]
    fn resident_pane() -> (Arc<dyn Pane>, RetireChild) {
        #[derive(Debug)]
        struct ResidentConfig;
        impl wezterm_term::TerminalConfiguration for ResidentConfig {
            fn color_palette(&self) -> wezterm_term::color::ColorPalette {
                Default::default()
            }
            fn scrollback_size(&self) -> usize {
                4096
            }
        }
        let mut terminal = wezterm_term::Terminal::new(
            wezterm_term::TerminalSize {
                rows: 4,
                cols: 24,
                dpi: 96,
                pixel_width: 192,
                pixel_height: 64,
            },
            Arc::new(ResidentConfig),
            "selection-extraction-resident-test",
            "test",
            Box::new(Vec::<u8>::new()),
        );
        write_history(&mut terminal, || {});
        local_pane(998_312, terminal)
    }

    /// A selection of `pane` from `start` to `end` with the pane's current
    /// layout authority. `anchored` gives it a native anchor, as the GUI's
    /// linear selections have when a copy starts; without one the worker
    /// fences the copy by the pane's sequence, as for a rectangular
    /// selection. A test pane has no mux, through which an anchor on a
    /// tiered history resolves, so only [`resident_pane`] is anchored.
    #[cfg(unix)]
    fn selection(
        pane: &Arc<dyn Pane>,
        start: SelectionCoordinate,
        end: SelectionCoordinate,
        rectangular: bool,
        anchored: bool,
    ) -> Selection {
        let (authority, sequence, _) = SelectionAuthority::capture_source(&**pane).unwrap();
        let mut desired = Selection::default();
        desired.origin = Some(start);
        desired.range = Some(SelectionRange { start, end });
        desired.seqno = sequence;
        desired.authority = Some(authority);
        desired.rectangular = rectangular;
        if !anchored {
            return desired;
        }
        let mut pending = PendingNativeSelection::new(desired.clone());
        let deadline = Instant::now() + Duration::from_secs(20);
        let token = loop {
            match crate::termwindow::TermWindow::capture_native_selection(pane, &mut pending) {
                crate::selection::NativeSelectionCapture::Ready(token) => break token,
                crate::selection::NativeSelectionCapture::Busy => {
                    assert!(
                        Instant::now() < deadline,
                        "the anchor capture never finished"
                    );
                    std::thread::sleep(Duration::from_millis(1));
                }
                _ => panic!("the fixture selection must be anchored"),
            }
        };
        desired.remember_native_anchor(token);
        assert!(desired.native_anchor().is_some());
        desired
    }

    /// The text the synchronous path makes of `desired`.
    #[cfg(unix)]
    fn synchronous_text(pane: &Arc<dyn Pane>, desired: &Selection) -> String {
        let range = desired.range.unwrap().normalize();
        super::super::selected_text_from_logical_lines(
            &pane.get_logical_lines(range.rows()),
            range,
            desired.rectangular,
        )
    }

    /// The last row of `pane`'s viewport.
    #[cfg(unix)]
    fn last_row(pane: &Arc<dyn Pane>) -> StableRowIndex {
        let dimensions = pane.get_dimensions();
        dimensions.physical_top + dimensions.viewport_rows as StableRowIndex - 1
    }

    /// ft-yccm0.2.2.4: the worker's slices make exactly the synchronous
    /// path's text, whatever the slice size, across cold, warm and hot rows:
    /// linear selections starting and ending inside wrapped lines and wide
    /// or combined cells, and rectangular ones. These have no native anchor
    /// (a cold-row anchor resolves through the mux), so each slice is
    /// fenced by the pane's sequence.
    #[test]
    #[cfg(unix)]
    fn worker_text_equals_the_synchronous_text_across_cold_warm_and_hot_rows() {
        let tiered = tiered_pane();
        let pane = &tiered.pane;
        let last = last_row(pane);
        let resident = tiered.resident_top;
        let at = SelectionCoordinate::x_y;
        assert_worker_parity(
            pane,
            &[
                // Every tier.
                (at(0, 0), at(23, last), false),
                (at(5, 1), at(7, last - 2), false),
                (at(2, 10), at(12, last - 1), true),
                // Cold only, and across the cold and warm seam.
                (at(3, 20), at(9, 41), false),
                (at(4, resident - 9), at(6, resident + 9), false),
            ],
            false,
        );
        let whole = synchronous_text(pane, &selection(pane, at(0, 0), at(23, last), false, false));
        assert!(whole.contains("界e\u{301}界e\u{301} wide") && whole.ends_with("END 界e\u{301}"));
    }

    /// ft-yccm0.2.2.4: the same parity with a native anchor proving every
    /// slice, as for the GUI's linear selections, over a history held in
    /// memory.
    #[test]
    #[cfg(unix)]
    fn anchored_worker_text_equals_the_synchronous_text() {
        let (pane, _child) = resident_pane();
        let last = last_row(&pane);
        let at = SelectionCoordinate::x_y;
        assert_worker_parity(
            &pane,
            &[
                (at(0, 0), at(23, last), false),
                (at(5, 1), at(7, last - 2), false),
                (at(3, 20), at(9, 41), false),
                (at(6, last - 30), at(2, last), false),
            ],
            true,
        );
    }

    /// Each selection, anchored or not, in slices of 1, 7, 64 and
    /// [`SLICE_ROWS`] rows, gives the synchronous path's text.
    #[cfg(unix)]
    fn assert_worker_parity(
        pane: &Arc<dyn Pane>,
        selections: &[(SelectionCoordinate, SelectionCoordinate, bool)],
        anchored: bool,
    ) {
        for &(start, end, rectangular) in selections {
            let desired = selection(pane, start, end, rectangular, anchored);
            let expected = synchronous_text(pane, &desired);
            assert!(expected.contains('\n'), "{start:?}..{end:?}");
            for slice_rows in [1, 7, 64, SLICE_ROWS] {
                let text = job(&desired, slice_rows)
                    .run(&Arc::downgrade(pane), &AtomicBool::new(false))
                    .unwrap_or_else(|reason| {
                        panic!("{start:?}..{end:?} by {slice_rows}: {reason}")
                    });
                assert_eq!(
                    text, expected,
                    "{start:?}..{end:?} rectangular={rectangular} anchored={anchored} \
                     in slices of {slice_rows}"
                );
            }
        }
    }

    /// ft-yccm0.2.2.4: a cancelled job stops before reading, a closed pane
    /// ends it, and dropping an extraction cancels its worker.
    #[test]
    #[cfg(unix)]
    fn cancellation_and_a_closed_pane_end_the_job() {
        let Tiered {
            pane, child, store, ..
        } = tiered_pane();
        let at = SelectionCoordinate::x_y;
        let desired = selection(&pane, at(0, 0), at(5, 30), false, false);
        assert_eq!(
            job(&desired, 4).run(&Arc::downgrade(&pane), &AtomicBool::new(true)),
            Err(STOPPED)
        );

        let closed = Arc::downgrade(&pane);
        drop(child);
        drop(pane);
        drop(store);
        assert_eq!(
            job(&desired, 4).run(&closed, &AtomicBool::new(false)),
            Err("The pane closed while copying.")
        );
        let (sender, receiver) = std::sync::mpsc::channel();
        let extraction = SelectionExtraction::start(
            closed,
            desired.clone(),
            SelectionCopy::new(&desired, desired.seqno).unwrap(),
            move |generation| sender.send(generation).unwrap(),
        )
        .unwrap();
        let generation = receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(generation, extraction.generation());
        assert_eq!(
            extraction.take_outcome(),
            Some(Err("The pane closed while copying."))
        );
        assert_eq!(extraction.take_outcome(), None);

        let cancelled = Arc::clone(&extraction.cancelled);
        drop(extraction);
        assert!(cancelled.load(Ordering::Acquire));
    }

    /// ft-yccm0.2.2.4: a completion takes the text only for its own
    /// generation and finished outcome, and copies it only if the selection
    /// is the one copied; a finished copy is retired either way.
    #[test]
    fn a_completion_serves_only_its_generation_and_selection() {
        let mut desired = Selection::default();
        desired.range = Some(SelectionRange {
            start: SelectionCoordinate::x_y(0, 0),
            end: SelectionCoordinate::x_y(4, 2),
        });
        let pending = |outcome: Option<Outcome>| {
            let mut copy = SelectionCopy::new(&desired, 0).unwrap();
            copy.extraction = Some(SelectionExtraction {
                generation: 7,
                cancelled: Arc::new(AtomicBool::new(false)),
                outcome: Arc::new(Mutex::new(outcome)),
            });
            let mut pending = PendingNativeSelection::new(desired.clone());
            pending.copy = Some(ClipboardCopyDestination::Clipboard);
            pending.text_copy = Some(copy);
            Some(pending)
        };

        // Another extraction's completion, or one still running: nothing.
        let mut running = pending(None);
        assert!(take_completed_copy(&mut running, &desired, 7).is_none());
        let mut other = pending(Some(Ok("text".to_owned())));
        assert!(take_completed_copy(&mut other, &desired, 8).is_none());
        assert!(running.is_some() && other.is_some());

        let mut done = pending(Some(Ok("text".to_owned())));
        assert_eq!(
            take_completed_copy(&mut done, &desired, 7),
            Some((
                Ok("text".to_owned()),
                Some(ClipboardCopyDestination::Clipboard)
            ))
        );
        assert!(done.is_none());

        let mut failed = pending(Some(Err("why")));
        assert_eq!(
            take_completed_copy(&mut failed, &desired, 7),
            Some((Err("why"), Some(ClipboardCopyDestination::Clipboard)))
        );

        // A new selection: the finished copy is retired, its text dropped.
        let mut stale = pending(Some(Ok("text".to_owned())));
        let mut moved = desired.clone();
        moved.range = Some(SelectionRange {
            start: SelectionCoordinate::x_y(1, 0),
            end: SelectionCoordinate::x_y(4, 2),
        });
        assert!(take_completed_copy(&mut stale, &moved, 7).is_none());
        assert!(stale.is_none());
    }
}
