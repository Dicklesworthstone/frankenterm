use crate::pane::{ForEachPaneLogicalLine, WithPaneLines};
use frankenterm_dynamic::{FromDynamic, ToDynamic};
use frankenterm_term::{
    Line, StableRowIndex, Terminal, TieredScrollbackStatus as TermTieredScrollbackStatus,
};
#[cfg(feature = "lua")]
use luahelper::impl_lua_conversion_dynamic;
use rangeset::RangeSet;
use serde::{Deserialize, Serialize};
use std::ops::Range;
use termwiz::hyperlink::Rule;
use termwiz::surface::SequenceNo;

/// Describes the location of the cursor
#[derive(
    Debug, Default, Copy, Clone, Hash, Eq, PartialEq, Deserialize, Serialize, FromDynamic, ToDynamic,
)]
pub struct StableCursorPosition {
    pub x: usize,
    pub y: StableRowIndex,
    pub shape: termwiz::surface::CursorShape,
    pub visibility: termwiz::surface::CursorVisibility,
}
#[cfg(feature = "lua")]
impl_lua_conversion_dynamic!(StableCursorPosition);

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, FromDynamic, ToDynamic,
)]
pub struct PaneTieredScrollbackStatus {
    pub tiering_enabled: bool,
    pub configured_scrollback_rows: usize,
    pub configured_hot_lines: usize,
    pub configured_warm_max_bytes: usize,
    pub visible_rows: usize,
    pub in_memory_scrollback_rows: usize,
    pub warm_resident_lines: usize,
    pub warm_resident_bytes: usize,
    pub warm_spill_lines_total: u64,
    pub warm_spill_bytes_total: u64,
    pub cold_spill_lines_total: u64,
    pub cold_spill_bytes_total: u64,
    pub cold_sink_retained_lines: usize,
    pub cold_sink_retained_bytes: usize,
    pub cold_worker_peak_backlog_depth: usize,
    pub cold_worker_completion_throughput_lines_per_sec: u64,
    pub cold_worker_completed_lines_total: u64,
    pub cold_worker_completed_batches_total: u64,
    pub cold_worker_cancellation_count: u64,
}
#[cfg(feature = "lua")]
impl_lua_conversion_dynamic!(PaneTieredScrollbackStatus);

impl From<TermTieredScrollbackStatus> for PaneTieredScrollbackStatus {
    fn from(status: TermTieredScrollbackStatus) -> Self {
        Self {
            tiering_enabled: status.tiering_enabled,
            configured_scrollback_rows: status.configured_scrollback_rows,
            configured_hot_lines: status.configured_hot_lines,
            configured_warm_max_bytes: status.configured_warm_max_bytes,
            visible_rows: status.visible_rows,
            in_memory_scrollback_rows: status.in_memory_scrollback_rows,
            warm_resident_lines: status.warm_resident_lines,
            warm_resident_bytes: status.warm_resident_bytes,
            warm_spill_lines_total: status.warm_spill_lines_total,
            warm_spill_bytes_total: status.warm_spill_bytes_total,
            cold_spill_lines_total: status.cold_spill_lines_total,
            cold_spill_bytes_total: status.cold_spill_bytes_total,
            cold_sink_retained_lines: status.cold_sink_retained_lines,
            cold_sink_retained_bytes: status.cold_sink_retained_bytes,
            cold_worker_peak_backlog_depth: status.cold_worker_peak_backlog_depth,
            cold_worker_completion_throughput_lines_per_sec: status
                .cold_worker_completion_throughput_lines_per_sec,
            cold_worker_completed_lines_total: status.cold_worker_completed_lines_total,
            cold_worker_completed_batches_total: status.cold_worker_completed_batches_total,
            cold_worker_cancellation_count: status.cold_worker_cancellation_count,
        }
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, FromDynamic, ToDynamic,
)]
pub struct RenderableDimensions {
    /// The viewport width
    pub cols: usize,
    /// How many rows fit in the viewport
    pub viewport_rows: usize,
    /// The total number of lines in the scrollback, including the viewport
    pub scrollback_rows: usize,

    /// The top of the physical, non-scrollback, screen expressed
    /// as a stable index.  It is envisioned that this will be used
    /// to compute row/cols for mouse events and to produce a range
    /// for the `get_lines` call when the scroll position is at the
    /// bottom of the screen.
    pub physical_top: StableRowIndex,
    /// The top of the scrollback (the earliest row we remember)
    /// expressed as a stable index.
    pub scrollback_top: StableRowIndex,
    pub dpi: u32,
    pub pixel_width: usize,
    pub pixel_height: usize,
    /// True if the lines should be rendered reversed
    pub reverse_video: bool,
}
#[cfg(feature = "lua")]
impl_lua_conversion_dynamic!(RenderableDimensions);

/// Implements Pane::get_cursor_position for Terminal
pub fn terminal_get_cursor_position(term: &mut Terminal) -> StableCursorPosition {
    let pos = term.cursor_pos();

    StableCursorPosition {
        x: pos.x,
        y: term.screen().visible_row_to_stable_row(pos.y),
        shape: pos.shape,
        visibility: pos.visibility,
    }
}

/// Implements Pane::get_dirty_lines for Terminal
pub fn terminal_get_dirty_lines(
    term: &mut Terminal,
    lines: Range<StableRowIndex>,
    seqno: SequenceNo,
) -> RangeSet<StableRowIndex> {
    let screen = term.screen();
    let lines = screen.get_changed_stable_rows(lines, seqno);
    let mut set = RangeSet::new();
    for line in lines {
        set.add(line);
    }
    set
}

pub fn terminal_for_each_logical_line_in_stable_range_mut(
    term: &mut Terminal,
    lines: Range<StableRowIndex>,
    for_line: &mut dyn ForEachPaneLogicalLine,
) {
    let screen = term.screen_mut();
    screen.for_each_logical_line_in_stable_range_mut(lines, |stable_range, lines| {
        for_line.with_logical_line_mut(stable_range, lines)
    });
}

/// Implements Pane::with_lines for Terminal
pub fn terminal_with_lines<F>(term: &mut Terminal, lines: Range<StableRowIndex>, mut func: F)
where
    F: FnMut(StableRowIndex, &[&Line]),
{
    let screen = term.screen_mut();
    let phys_range = screen.stable_range(&lines);
    let first = screen.phys_to_stable_row_index(phys_range.start);

    screen.with_phys_lines(phys_range, |lines| func(first, lines));
}

/// Implements Pane::with_lines_mut for Terminal
pub fn terminal_with_lines_mut(
    term: &mut Terminal,
    lines: Range<StableRowIndex>,
    with_lines: &mut dyn WithPaneLines,
) {
    let screen = term.screen_mut();
    let phys_range = screen.stable_range(&lines);
    let first = screen.phys_to_stable_row_index(phys_range.start);

    screen.with_phys_lines_mut(phys_range, |lines| with_lines.with_lines_mut(first, lines));
}

/// Apply implicit hyperlink rules and expose the requested physical rows while
/// one caller-owned terminal guard remains held. This prevents terminal
/// mutation between the logical-line scan and the render callback and avoids a
/// second terminal-lock acquisition on the paint hot path.
pub fn terminal_with_lines_mut_and_apply_hyperlinks(
    term: &mut Terminal,
    lines: Range<StableRowIndex>,
    rules: &[Rule],
    with_lines: &mut dyn WithPaneLines,
) {
    struct ApplyHyperlinks<'a> {
        rules: &'a [Rule],
    }

    impl ForEachPaneLogicalLine for ApplyHyperlinks<'_> {
        fn with_logical_line_mut(
            &mut self,
            _stable_range: Range<StableRowIndex>,
            lines: &mut [&mut Line],
        ) -> bool {
            Line::apply_hyperlink_rules(self.rules, lines);
            true
        }
    }

    terminal_for_each_logical_line_in_stable_range_mut(
        term,
        lines.clone(),
        &mut ApplyHyperlinks { rules },
    );
    terminal_with_lines_mut(term, lines, with_lines);
}

/// Implements Pane::get_lines for Terminal
pub fn terminal_get_lines(
    term: &mut Terminal,
    lines: Range<StableRowIndex>,
) -> (StableRowIndex, Vec<Line>) {
    let screen = term.screen_mut();
    screen.lines_in_stable_range(lines)
}

/// Implements Pane::get_dimensions for Terminal
pub fn terminal_get_dimensions(term: &mut Terminal) -> RenderableDimensions {
    let size = term.get_size();
    let screen = term.screen();
    RenderableDimensions {
        cols: screen.physical_cols,
        viewport_rows: screen.physical_rows,
        scrollback_rows: screen.reachable_scrollback_rows(),
        physical_top: screen.visible_row_to_stable_row(0),
        scrollback_top: screen.scrollback_top_stable_row(),
        dpi: screen.dpi,
        pixel_width: size.pixel_width,
        pixel_height: size.pixel_height,
        reverse_video: term.get_reverse_video(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_cursor_position_default() {
        let pos = StableCursorPosition::default();
        assert_eq!(pos.x, 0);
        assert_eq!(pos.y, 0);
    }

    #[test]
    fn stable_cursor_position_equality() {
        let a = StableCursorPosition::default();
        let b = StableCursorPosition::default();
        assert_eq!(a, b);
    }

    #[test]
    fn stable_cursor_position_inequality() {
        let a = StableCursorPosition::default();
        let b = StableCursorPosition {
            x: 5,
            ..Default::default()
        };
        assert_ne!(a, b);
    }

    #[test]
    fn stable_cursor_position_clone_copy() {
        let a = StableCursorPosition {
            x: 10,
            y: 20,
            ..Default::default()
        };
        let b = a; // Copy
        let c = a.clone(); // Clone
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn stable_cursor_position_debug() {
        let pos = StableCursorPosition {
            x: 5,
            y: 10,
            ..Default::default()
        };
        let dbg = format!("{:?}", pos);
        assert!(dbg.contains("StableCursorPosition"));
        assert!(dbg.contains("5"));
        assert!(dbg.contains("10"));
    }

    #[test]
    fn stable_cursor_position_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(StableCursorPosition::default());
        set.insert(StableCursorPosition {
            x: 1,
            ..Default::default()
        });
        set.insert(StableCursorPosition::default()); // duplicate
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn renderable_dimensions_default() {
        let dims = RenderableDimensions::default();
        assert_eq!(dims.cols, 0);
        assert_eq!(dims.viewport_rows, 0);
        assert_eq!(dims.scrollback_rows, 0);
        assert_eq!(dims.physical_top, 0);
        assert_eq!(dims.scrollback_top, 0);
        assert_eq!(dims.dpi, 0);
        assert!(!dims.reverse_video);
    }

    #[test]
    fn renderable_dimensions_equality() {
        let a = RenderableDimensions::default();
        let b = RenderableDimensions::default();
        assert_eq!(a, b);
    }

    #[test]
    fn renderable_dimensions_inequality() {
        let a = RenderableDimensions::default();
        let b = RenderableDimensions {
            cols: 80,
            viewport_rows: 24,
            ..Default::default()
        };
        assert_ne!(a, b);
    }

    #[test]
    fn renderable_dimensions_clone_copy() {
        let a = RenderableDimensions {
            cols: 120,
            viewport_rows: 40,
            scrollback_rows: 10000,
            physical_top: 9960,
            scrollback_top: 0,
            dpi: 96,
            pixel_width: 960,
            pixel_height: 640,
            reverse_video: false,
        };
        let b = a; // Copy
        let c = a.clone(); // Clone
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn renderable_dimensions_debug() {
        let dims = RenderableDimensions {
            cols: 80,
            viewport_rows: 24,
            ..Default::default()
        };
        let dbg = format!("{:?}", dims);
        assert!(dbg.contains("RenderableDimensions"));
        assert!(dbg.contains("80"));
        assert!(dbg.contains("24"));
    }

    #[test]
    fn renderable_dimensions_with_reverse_video() {
        let dims = RenderableDimensions {
            reverse_video: true,
            ..Default::default()
        };
        assert!(dims.reverse_video);
        assert_ne!(dims, RenderableDimensions::default());
    }

    #[test]
    fn pane_tiered_scrollback_status_converts_from_terminal_status() {
        let status = PaneTieredScrollbackStatus::from(TermTieredScrollbackStatus {
            tiering_enabled: true,
            configured_scrollback_rows: 128,
            configured_hot_lines: 64,
            configured_warm_max_bytes: 4096,
            visible_rows: 32,
            in_memory_scrollback_rows: 48,
            warm_resident_lines: 16,
            warm_resident_bytes: 2048,
            warm_spill_lines_total: 99,
            warm_spill_bytes_total: 8192,
            cold_spill_lines_total: 55,
            cold_spill_bytes_total: 16384,
            cold_sink_retained_lines: 8,
            cold_sink_retained_bytes: 4096,
            cold_worker_peak_backlog_depth: 3,
            cold_worker_completion_throughput_lines_per_sec: 777,
            cold_worker_completed_lines_total: 44,
            cold_worker_completed_batches_total: 11,
            cold_worker_cancellation_count: 2,
        });

        assert!(status.tiering_enabled);
        assert_eq!(status.configured_hot_lines, 64);
        assert_eq!(status.warm_resident_bytes, 2048);
        assert_eq!(status.cold_sink_retained_lines, 8);
        assert_eq!(status.cold_worker_completed_batches_total, 11);
    }

    /// Executable reference model for the immutable render-snapshot
    /// publication contract in `ft-interactive-systems-performance-4tenz.6.2`.
    ///
    /// This deliberately remains test-only: `.6.3` owns the live producer and
    /// renderer cutover. Keeping the model here lets the mux test target prove
    /// the identity, last-known-good, cancellation, budget, and exhaustion
    /// rules without pretending that a dormant buffer is a production path.
    mod render_snapshot_publication_model {
        const COMPLETE_FIELDS: u16 = 0b1_1111_1111_1111;
        const PUBLICATION_EXHAUSTED: u8 = 7;

        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        struct ExactTarget {
            session: u8,
            window_incarnation: u8,
            tab_incarnation: u8,
            pane_incarnation: u8,
            topology_generation: u8,
            geometry_generation: u8,
            alternate_screen_generation: u8,
            overlay_generation: u8,
        }

        impl ExactTarget {
            const fn initial() -> Self {
                Self {
                    session: 1,
                    window_incarnation: 1,
                    tab_incarnation: 1,
                    pane_incarnation: 1,
                    topology_generation: 1,
                    geometry_generation: 1,
                    alternate_screen_generation: 1,
                    overlay_generation: 1,
                }
            }
        }

        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        struct Candidate {
            target: ExactTarget,
            source_generation: u8,
            complete_fields: u16,
            retained_bytes: u8,
            digest: u8,
        }

        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        struct Published {
            target: ExactTarget,
            source_generation: u8,
            publication_generation: u8,
            complete_fields: u16,
            retained_bytes: u8,
            digest: u8,
        }

        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        enum CommitOutcome {
            Published(u8),
            NoChange(u8),
            Rejected,
            Exhausted,
        }

        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        enum Event {
            BeginValid,
            BeginIncomplete,
            BeginOverBudget,
            Commit,
            Cancel,
            AcquireRender,
            ReleaseRender,
            ContentMutation,
            ImageHyperlinkMutation,
            TopologyMutation,
            Resize,
            AlternateScreenSwitch,
            OverlayMutation,
            SelectionImeMutation,
            PaneReplacement,
            Reconnect,
            Detach,
        }

        const EVENTS: [Event; 17] = [
            Event::BeginValid,
            Event::BeginIncomplete,
            Event::BeginOverBudget,
            Event::Commit,
            Event::Cancel,
            Event::AcquireRender,
            Event::ReleaseRender,
            Event::ContentMutation,
            Event::ImageHyperlinkMutation,
            Event::TopologyMutation,
            Event::Resize,
            Event::AlternateScreenSwitch,
            Event::OverlayMutation,
            Event::SelectionImeMutation,
            Event::PaneReplacement,
            Event::Reconnect,
            Event::Detach,
        ];

        #[derive(Clone, Debug, Eq, PartialEq)]
        struct PublicationModel {
            target: ExactTarget,
            source_generation: u8,
            publication_generation: u8,
            retained_byte_budget: u8,
            attached: bool,
            exhausted: bool,
            pending: Option<Candidate>,
            published: Option<Published>,
            rendering: Option<Published>,
        }

        impl PublicationModel {
            fn new(retained_byte_budget: u8) -> Self {
                Self {
                    target: ExactTarget::initial(),
                    source_generation: 0,
                    publication_generation: 0,
                    retained_byte_budget,
                    attached: true,
                    exhausted: false,
                    pending: None,
                    published: None,
                    rendering: None,
                }
            }

            fn fail_exhausted(&mut self) {
                self.exhausted = true;
                self.pending = None;
                self.published = None;
            }

            fn retained_generation_bytes(&self, candidate_bytes: Option<u8>) -> usize {
                let candidate =
                    candidate_bytes.or_else(|| self.pending.map(|pending| pending.retained_bytes));
                let mut total = candidate.map_or(0, usize::from);
                if let Some(published) = self.published {
                    total += usize::from(published.retained_bytes);
                }
                if let Some(rendering) = self.rendering {
                    let shares_published = self.published.is_some_and(|published| {
                        published.target == rendering.target
                            && published.publication_generation == rendering.publication_generation
                    });
                    if !shares_published {
                        total += usize::from(rendering.retained_bytes);
                    }
                }
                total
            }

            fn begin(&mut self, complete_fields: u16, retained_bytes: u8, digest: u8) {
                if self.pending.is_none()
                    && !self.exhausted
                    && self.attached
                    && self.retained_generation_bytes(Some(retained_bytes))
                        <= usize::from(self.retained_byte_budget)
                {
                    self.pending = Some(Candidate {
                        target: self.target,
                        source_generation: self.source_generation,
                        complete_fields,
                        retained_bytes,
                        digest,
                    });
                }
            }

            fn commit(&mut self) -> CommitOutcome {
                if self.exhausted {
                    return CommitOutcome::Exhausted;
                }
                let Some(candidate) = self.pending.take() else {
                    return CommitOutcome::Rejected;
                };
                if !self.attached
                    || candidate.target != self.target
                    || candidate.source_generation != self.source_generation
                    || candidate.complete_fields != COMPLETE_FIELDS
                    || candidate.retained_bytes > self.retained_byte_budget
                {
                    return CommitOutcome::Rejected;
                }
                if let Some(published) = self.published {
                    if published.target == candidate.target
                        && published.source_generation == candidate.source_generation
                    {
                        if published.digest == candidate.digest {
                            return CommitOutcome::NoChange(published.publication_generation);
                        }
                        return CommitOutcome::Rejected;
                    }
                }

                let Some(next) = self.publication_generation.checked_add(1) else {
                    self.fail_exhausted();
                    return CommitOutcome::Exhausted;
                };
                if next == PUBLICATION_EXHAUSTED {
                    self.fail_exhausted();
                    return CommitOutcome::Exhausted;
                }
                self.publication_generation = next;
                self.published = Some(Published {
                    target: candidate.target,
                    source_generation: candidate.source_generation,
                    publication_generation: next,
                    complete_fields: candidate.complete_fields,
                    retained_bytes: candidate.retained_bytes,
                    digest: candidate.digest,
                });
                CommitOutcome::Published(next)
            }

            fn content_mutation(&mut self) {
                let Some(next) = self
                    .source_generation
                    .checked_add(1)
                    .filter(|next| *next != u8::MAX)
                else {
                    self.fail_exhausted();
                    return;
                };
                self.source_generation = next;
            }

            fn topology_mutation(&mut self) {
                let Some(next) = self
                    .target
                    .topology_generation
                    .checked_add(1)
                    .filter(|next| *next != u8::MAX)
                else {
                    self.fail_exhausted();
                    return;
                };
                self.target.topology_generation = next;
                self.published = None;
            }

            fn resize(&mut self) {
                let Some(next) = self
                    .target
                    .geometry_generation
                    .checked_add(1)
                    .filter(|next| *next != u8::MAX)
                else {
                    self.fail_exhausted();
                    return;
                };
                self.target.geometry_generation = next;
                self.published = None;
            }

            fn alternate_screen_switch(&mut self) {
                let Some(next) = self
                    .target
                    .alternate_screen_generation
                    .checked_add(1)
                    .filter(|next| *next != u8::MAX)
                else {
                    self.fail_exhausted();
                    return;
                };
                self.target.alternate_screen_generation = next;
                self.published = None;
            }

            fn overlay_mutation(&mut self) {
                let Some(next) = self
                    .target
                    .overlay_generation
                    .checked_add(1)
                    .filter(|next| *next != u8::MAX)
                else {
                    self.fail_exhausted();
                    return;
                };
                self.target.overlay_generation = next;
                self.published = None;
            }

            fn replace_pane(&mut self) {
                let Some(next) = self
                    .target
                    .pane_incarnation
                    .checked_add(1)
                    .filter(|next| *next != u8::MAX)
                else {
                    self.fail_exhausted();
                    return;
                };
                self.target.pane_incarnation = next;
                self.source_generation = 0;
                self.published = None;
            }

            fn reconnect(&mut self) {
                let Some(next_session) = self
                    .target
                    .session
                    .checked_add(1)
                    .filter(|next| *next != u8::MAX)
                else {
                    self.fail_exhausted();
                    return;
                };
                self.target.session = next_session;
                self.target.window_incarnation = 1;
                self.target.tab_incarnation = 1;
                self.target.pane_incarnation = 1;
                self.target.topology_generation = 1;
                self.target.geometry_generation = 1;
                self.target.alternate_screen_generation = 1;
                self.target.overlay_generation = 1;
                self.source_generation = 0;
                self.publication_generation = 0;
                self.attached = true;
                self.exhausted = false;
                self.pending = None;
                self.published = None;
            }

            fn detach(&mut self) {
                self.attached = false;
                self.pending = None;
                self.published = None;
            }

            fn acquire_render(&mut self) {
                if self.rendering.is_none() {
                    self.rendering = self.published;
                }
            }

            fn release_render(&mut self) -> bool {
                let Some(rendering) = self.rendering.take() else {
                    return false;
                };
                !self.exhausted
                    && self.attached
                    && rendering.target == self.target
                    && self.published.is_some_and(|published| {
                        published.target == rendering.target
                            && published.publication_generation == rendering.publication_generation
                    })
            }

            fn apply(&mut self, event: Event) {
                match event {
                    Event::BeginValid => self.begin(COMPLETE_FIELDS, 4, 1),
                    Event::BeginIncomplete => self.begin(COMPLETE_FIELDS ^ 1, 4, 2),
                    Event::BeginOverBudget => self.begin(COMPLETE_FIELDS, 9, 3),
                    Event::Commit => {
                        let _ = self.commit();
                    }
                    Event::Cancel => self.pending = None,
                    Event::AcquireRender => self.acquire_render(),
                    Event::ReleaseRender => {
                        let _ = self.release_render();
                    }
                    Event::ContentMutation => self.content_mutation(),
                    Event::ImageHyperlinkMutation => self.content_mutation(),
                    Event::TopologyMutation => self.topology_mutation(),
                    Event::Resize => self.resize(),
                    Event::AlternateScreenSwitch => self.alternate_screen_switch(),
                    Event::OverlayMutation => self.overlay_mutation(),
                    Event::SelectionImeMutation => self.overlay_mutation(),
                    Event::PaneReplacement => self.replace_pane(),
                    Event::Reconnect => self.reconnect(),
                    Event::Detach => self.detach(),
                }
                self.assert_invariants();
            }

            fn assert_invariants(&self) {
                assert!(self.publication_generation < PUBLICATION_EXHAUSTED);
                let retained_slots = usize::from(self.pending.is_some())
                    + usize::from(self.published.is_some())
                    + usize::from(self.rendering.is_some());
                assert!(retained_slots <= 3);
                assert!(
                    self.retained_generation_bytes(None) <= usize::from(self.retained_byte_budget)
                );
                if self.exhausted || !self.attached {
                    assert!(self.published.is_none());
                }
                if let Some(published) = self.published {
                    assert_eq!(published.target, self.target);
                    assert!(published.source_generation <= self.source_generation);
                    assert!(published.publication_generation > 0);
                    assert_eq!(published.complete_fields, COMPLETE_FIELDS);
                    assert!(published.retained_bytes <= self.retained_byte_budget);
                }
                if let Some(rendering) = self.rendering {
                    assert!(rendering.publication_generation > 0);
                    assert_eq!(rendering.complete_fields, COMPLETE_FIELDS);
                    assert!(rendering.retained_bytes <= self.retained_byte_budget);
                }
            }
        }

        #[test]
        fn last_known_good_survives_content_build_failure_but_not_geometry_or_identity_change() {
            let mut model = PublicationModel::new(8);
            model.begin(COMPLETE_FIELDS, 4, 1);
            assert_eq!(model.commit(), CommitOutcome::Published(1));
            let first = model.published;

            model.content_mutation();
            model.begin(COMPLETE_FIELDS ^ 1, 4, 2);
            assert_eq!(model.commit(), CommitOutcome::Rejected);
            assert_eq!(model.published, first);

            model.resize();
            assert!(model.published.is_none());
            model.begin(COMPLETE_FIELDS, 4, 3);
            model.replace_pane();
            assert_eq!(model.commit(), CommitOutcome::Rejected);
            assert!(model.published.is_none());
        }

        #[test]
        fn stale_incomplete_over_budget_and_same_source_equivocation_fail_closed() {
            let mut model = PublicationModel::new(8);
            model.begin(COMPLETE_FIELDS ^ 1, 4, 1);
            assert_eq!(model.commit(), CommitOutcome::Rejected);
            model.begin(COMPLETE_FIELDS, 9, 1);
            assert_eq!(model.commit(), CommitOutcome::Rejected);

            model.begin(COMPLETE_FIELDS, 4, 1);
            assert_eq!(model.commit(), CommitOutcome::Published(1));
            model.begin(COMPLETE_FIELDS, 4, 1);
            assert_eq!(model.commit(), CommitOutcome::NoChange(1));
            model.begin(COMPLETE_FIELDS, 4, 2);
            assert_eq!(model.commit(), CommitOutcome::Rejected);

            model.content_mutation();
            model.begin(COMPLETE_FIELDS, 4, 3);
            model.content_mutation();
            assert_eq!(model.commit(), CommitOutcome::Rejected);
        }

        #[test]
        fn publication_generation_exhaustion_is_terminal_and_clears_eligibility() {
            let mut model = PublicationModel::new(8);
            for source in 0..PUBLICATION_EXHAUSTED - 1 {
                let next_publication = source
                    .checked_add(1)
                    .expect("the bounded model source must have a successor");
                model.source_generation = source;
                model.begin(COMPLETE_FIELDS, 4, next_publication);
                assert_eq!(model.commit(), CommitOutcome::Published(next_publication));
            }
            model.source_generation = PUBLICATION_EXHAUSTED - 1;
            model.begin(COMPLETE_FIELDS, 4, PUBLICATION_EXHAUSTED);
            assert_eq!(model.commit(), CommitOutcome::Exhausted);
            assert!(model.exhausted);
            assert!(model.published.is_none());
            assert_eq!(model.commit(), CommitOutcome::Exhausted);

            model.reconnect();
            model.begin(COMPLETE_FIELDS, 4, 1);
            assert_eq!(model.commit(), CommitOutcome::Published(1));
        }

        #[test]
        fn every_named_render_state_class_has_explicit_stale_publication_semantics() {
            let mut model = PublicationModel::new(8);
            model.begin(COMPLETE_FIELDS, 4, 1);
            assert_eq!(model.commit(), CommitOutcome::Published(1));

            let last_known_good = model.published;
            model.apply(Event::ImageHyperlinkMutation);
            assert_eq!(model.published, last_known_good);
            model.begin(COMPLETE_FIELDS, 4, 2);
            model.apply(Event::AlternateScreenSwitch);
            assert_eq!(model.commit(), CommitOutcome::Rejected);
            assert!(model.published.is_none());

            model.begin(COMPLETE_FIELDS, 4, 3);
            assert_eq!(model.commit(), CommitOutcome::Published(2));
            model.begin(COMPLETE_FIELDS, 4, 4);
            model.apply(Event::SelectionImeMutation);
            assert_eq!(model.commit(), CommitOutcome::Rejected);
            assert!(model.published.is_none());

            model.begin(COMPLETE_FIELDS, 4, 5);
            assert_eq!(model.commit(), CommitOutcome::Published(3));
            model.begin(COMPLETE_FIELDS, 4, 6);
            model.apply(Event::TopologyMutation);
            assert_eq!(model.commit(), CommitOutcome::Rejected);
            assert!(model.published.is_none());
        }

        #[test]
        fn every_generation_domain_fails_closed_before_its_exhausted_sentinel() {
            fn assert_exhausted(mut model: PublicationModel, event: Event) {
                model.begin(COMPLETE_FIELDS, 4, 1);
                assert_eq!(model.commit(), CommitOutcome::Published(1));
                model.apply(event);
                assert!(model.exhausted);
                assert!(model.pending.is_none());
                assert!(model.published.is_none());
            }

            let mut source = PublicationModel::new(8);
            source.source_generation = u8::MAX - 1;
            assert_exhausted(source, Event::ContentMutation);

            let mut topology = PublicationModel::new(8);
            topology.target.topology_generation = u8::MAX - 1;
            assert_exhausted(topology, Event::TopologyMutation);

            let mut geometry = PublicationModel::new(8);
            geometry.target.geometry_generation = u8::MAX - 1;
            assert_exhausted(geometry, Event::Resize);

            let mut screen = PublicationModel::new(8);
            screen.target.alternate_screen_generation = u8::MAX - 1;
            assert_exhausted(screen, Event::AlternateScreenSwitch);

            let mut overlay = PublicationModel::new(8);
            overlay.target.overlay_generation = u8::MAX - 1;
            assert_exhausted(overlay, Event::SelectionImeMutation);

            let mut pane = PublicationModel::new(8);
            pane.target.pane_incarnation = u8::MAX - 1;
            assert_exhausted(pane, Event::PaneReplacement);

            let mut session = PublicationModel::new(8);
            session.target.session = u8::MAX - 1;
            assert_exhausted(session, Event::Reconnect);
        }

        #[test]
        fn newer_publication_supersedes_an_in_flight_frame_without_unbounded_generations() {
            let mut model = PublicationModel::new(8);
            model.begin(COMPLETE_FIELDS, 4, 1);
            assert_eq!(model.commit(), CommitOutcome::Published(1));
            model.acquire_render();
            let first_render = model.rendering;
            model.acquire_render();
            assert_eq!(model.rendering, first_render);

            model.content_mutation();
            model.begin(COMPLETE_FIELDS, 4, 2);
            assert_eq!(model.commit(), CommitOutcome::Published(2));
            model.content_mutation();
            model.begin(COMPLETE_FIELDS, 4, 3);
            assert!(model.pending.is_none());
            assert!(!model.release_render());
            assert_eq!(
                model
                    .published
                    .map(|snapshot| snapshot.publication_generation),
                Some(2)
            );

            model.acquire_render();
            assert!(model.release_render());
        }

        #[test]
        fn bounded_event_interleavings_preserve_publication_invariants() {
            fn visit(model: PublicationModel, depth: usize) {
                if depth == 0 {
                    return;
                }
                for event in EVENTS {
                    let mut next = model.clone();
                    next.apply(event);
                    visit(next, depth - 1);
                }
            }

            visit(PublicationModel::new(8), 5);
        }
    }
}
