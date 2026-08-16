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
        use std::collections::{BTreeMap, BTreeSet};
        use std::sync::{Arc, Barrier, Mutex};

        const PUBLICATION_EXHAUSTED: u8 = 7;
        const CANDIDATE_LIFETIME_TICKS: u8 = 3;

        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        #[repr(u8)]
        enum RequiredField {
            WindowTopology,
            PaneIdentity,
            VisibleRowsAndCells,
            HyperlinksAndImages,
            Cursor,
            TerminalMetadata,
            SemanticZones,
            GuiOverlay,
            Prediction,
            ImeAccessibility,
            SynchronizedOutputAndCompositing,
            FontConfigAndCacheEpochs,
            DamageSettlement,
            ResourceUsage,
            RemoteIdentity,
        }

        const REQUIRED_FIELDS: [RequiredField; 15] = [
            RequiredField::WindowTopology,
            RequiredField::PaneIdentity,
            RequiredField::VisibleRowsAndCells,
            RequiredField::HyperlinksAndImages,
            RequiredField::Cursor,
            RequiredField::TerminalMetadata,
            RequiredField::SemanticZones,
            RequiredField::GuiOverlay,
            RequiredField::Prediction,
            RequiredField::ImeAccessibility,
            RequiredField::SynchronizedOutputAndCompositing,
            RequiredField::FontConfigAndCacheEpochs,
            RequiredField::DamageSettlement,
            RequiredField::ResourceUsage,
            RequiredField::RemoteIdentity,
        ];

        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        struct FieldSet(u32);

        impl FieldSet {
            const COMPLETE: Self = Self((1 << REQUIRED_FIELDS.len()) - 1);

            const fn bit(field: RequiredField) -> u32 {
                1 << field as u8
            }

            const fn without(self, field: RequiredField) -> Self {
                Self(self.0 & !Self::bit(field))
            }

            fn missing(self) -> Vec<RequiredField> {
                REQUIRED_FIELDS
                    .iter()
                    .copied()
                    .filter(|field| self.0 & Self::bit(*field) == 0)
                    .collect()
            }

            const fn is_complete(self) -> bool {
                self.0 == Self::COMPLETE.0
            }
        }

        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        struct ExactTarget {
            session: u8,
            window_incarnation: u8,
            window_order_generation: u8,
            tab_incarnation: u8,
            pane_incarnation: u8,
            topology_generation: u8,
            geometry_generation: u8,
            viewport_generation: u8,
            alternate_screen_generation: u8,
            overlay_generation: u8,
            selection_ime_generation: u8,
            prediction_generation: u8,
            synchronized_output_generation: u8,
            font_config_generation: u8,
            renderer_cache_generation: u8,
            device_generation: u8,
            damage_generation: u8,
            remote_connection_incarnation: u8,
            remote_delivery_generation: u8,
        }

        impl ExactTarget {
            const fn initial() -> Self {
                Self {
                    session: 1,
                    window_incarnation: 1,
                    window_order_generation: 1,
                    tab_incarnation: 1,
                    pane_incarnation: 1,
                    topology_generation: 1,
                    geometry_generation: 1,
                    viewport_generation: 1,
                    alternate_screen_generation: 1,
                    overlay_generation: 1,
                    selection_ime_generation: 1,
                    prediction_generation: 1,
                    synchronized_output_generation: 1,
                    font_config_generation: 1,
                    renderer_cache_generation: 1,
                    device_generation: 1,
                    damage_generation: 1,
                    remote_connection_incarnation: 1,
                    remote_delivery_generation: 1,
                }
            }
        }

        #[derive(Clone, Debug)]
        struct Candidate {
            target: ExactTarget,
            source_generation: u8,
            fields: FieldSet,
            retained_bytes: u8,
            hidden_tab_count: u8,
            hidden_tab_metadata_bytes: u8,
            age_ticks: u8,
            digest: u8,
            strong_capture: Arc<()>,
            reservation: WindowReservation,
        }

        #[derive(Clone, Debug, Eq, PartialEq)]
        struct Published {
            target: ExactTarget,
            source_generation: u8,
            publication_generation: u8,
            fields: FieldSet,
            retained_bytes: u8,
            hidden_tab_count: u8,
            hidden_tab_metadata_bytes: u8,
            digest: u8,
            reservation: WindowReservation,
        }

        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        enum CommitOutcome {
            Published(u8),
            NoChange(u8),
            Rejected,
            Exhausted,
        }

        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        enum GenerationDomain {
            Source,
            WindowIncarnation,
            WindowOrder,
            TabIncarnation,
            PaneIncarnation,
            Topology,
            Geometry,
            Viewport,
            AlternateScreen,
            Overlay,
            SelectionIme,
            Prediction,
            SynchronizedOutput,
            FontConfig,
            RendererCache,
            Device,
            Damage,
            RemoteConnection,
            RemoteDelivery,
        }

        const GENERATION_DOMAINS: [GenerationDomain; 19] = [
            GenerationDomain::Source,
            GenerationDomain::WindowIncarnation,
            GenerationDomain::WindowOrder,
            GenerationDomain::TabIncarnation,
            GenerationDomain::PaneIncarnation,
            GenerationDomain::Topology,
            GenerationDomain::Geometry,
            GenerationDomain::Viewport,
            GenerationDomain::AlternateScreen,
            GenerationDomain::Overlay,
            GenerationDomain::SelectionIme,
            GenerationDomain::Prediction,
            GenerationDomain::SynchronizedOutput,
            GenerationDomain::FontConfig,
            GenerationDomain::RendererCache,
            GenerationDomain::Device,
            GenerationDomain::Damage,
            GenerationDomain::RemoteConnection,
            GenerationDomain::RemoteDelivery,
        ];

        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        enum Event {
            BeginValid,
            BeginIncomplete,
            BeginUnresolvedImage,
            BeginOverBudget,
            BeginTooManyTabs,
            BeginTabMetadataOverBudget,
            Commit,
            CancelBeforePublication,
            CancelAfterPublication,
            Tick,
            AcquireRender,
            ReleaseRender,
            Advance(GenerationDomain),
            ImageHyperlinkMutation,
            Reconnect,
            Detach,
        }

        const EVENTS: [Event; 34] = [
            Event::BeginValid,
            Event::BeginIncomplete,
            Event::BeginUnresolvedImage,
            Event::BeginOverBudget,
            Event::BeginTooManyTabs,
            Event::BeginTabMetadataOverBudget,
            Event::Commit,
            Event::CancelBeforePublication,
            Event::CancelAfterPublication,
            Event::Tick,
            Event::AcquireRender,
            Event::ReleaseRender,
            Event::Advance(GenerationDomain::Source),
            Event::ImageHyperlinkMutation,
            Event::Advance(GenerationDomain::WindowIncarnation),
            Event::Advance(GenerationDomain::WindowOrder),
            Event::Advance(GenerationDomain::TabIncarnation),
            Event::Advance(GenerationDomain::PaneIncarnation),
            Event::Advance(GenerationDomain::Topology),
            Event::Advance(GenerationDomain::Geometry),
            Event::Advance(GenerationDomain::Viewport),
            Event::Advance(GenerationDomain::AlternateScreen),
            Event::Advance(GenerationDomain::Overlay),
            Event::Advance(GenerationDomain::SelectionIme),
            Event::Advance(GenerationDomain::Prediction),
            Event::Advance(GenerationDomain::SynchronizedOutput),
            Event::Advance(GenerationDomain::FontConfig),
            Event::Advance(GenerationDomain::RendererCache),
            Event::Advance(GenerationDomain::Device),
            Event::Advance(GenerationDomain::Damage),
            Event::Advance(GenerationDomain::RemoteConnection),
            Event::Advance(GenerationDomain::RemoteDelivery),
            Event::Reconnect,
            Event::Detach,
        ];

        #[derive(Clone, Debug)]
        struct PublicationModel {
            target: ExactTarget,
            source_generation: u8,
            publication_generation: u8,
            retained_byte_budget: u8,
            max_hidden_tabs: u8,
            max_hidden_tab_metadata_bytes: u8,
            candidate_lifetime_ticks: u8,
            arena: SessionSnapshotBudget,
            publisher_id: u16,
            attached: bool,
            exhausted: bool,
            pending: Option<Candidate>,
            published: Option<Published>,
            rendering: Option<Published>,
        }

        impl PublicationModel {
            fn new(retained_byte_budget: u8) -> Self {
                let mut arena =
                    SessionSnapshotBudget::new(1, usize::from(retained_byte_budget), 9, 12, 1);
                let publisher_id = arena
                    .register_publisher()
                    .expect("the single-window model must register its publisher");
                Self {
                    target: ExactTarget::initial(),
                    source_generation: 0,
                    publication_generation: 0,
                    retained_byte_budget,
                    max_hidden_tabs: 3,
                    max_hidden_tab_metadata_bytes: 4,
                    candidate_lifetime_ticks: CANDIDATE_LIFETIME_TICKS,
                    arena,
                    publisher_id,
                    attached: true,
                    exhausted: false,
                    pending: None,
                    published: None,
                    rendering: None,
                }
            }

            fn fail_exhausted(&mut self) {
                self.exhausted = true;
                self.drop_candidate();
                self.replace_published(None);
            }

            fn drop_candidate(&mut self) {
                if let Some(candidate) = self.pending.take() {
                    self.arena
                        .release(candidate.reservation)
                        .expect("a pending candidate must own one live reservation");
                }
            }

            fn retained_generation_bytes(&self) -> usize {
                let mut total = self
                    .pending
                    .as_ref()
                    .map_or(0, |pending| usize::from(pending.retained_bytes));
                if let Some(published) = &self.published {
                    total += usize::from(published.retained_bytes);
                }
                if let Some(rendering) = &self.rendering {
                    let shares_published = self.published.as_ref().is_some_and(|published| {
                        published.reservation.token_id == rendering.reservation.token_id
                    });
                    if !shares_published {
                        total += usize::from(rendering.retained_bytes);
                    }
                }
                total
            }

            fn begin(
                &mut self,
                fields: FieldSet,
                retained_bytes: u8,
                hidden_tab_count: u8,
                hidden_tab_metadata_bytes: u8,
                digest: u8,
            ) {
                if self.pending.is_none()
                    && !self.exhausted
                    && self.attached
                    && hidden_tab_count <= self.max_hidden_tabs
                    && hidden_tab_metadata_bytes <= self.max_hidden_tab_metadata_bytes
                {
                    let Some(reservation) = self.arena.try_admit(
                        self.publisher_id,
                        usize::from(retained_bytes),
                        usize::from(hidden_tab_count),
                        usize::from(hidden_tab_metadata_bytes),
                    ) else {
                        return;
                    };
                    self.pending = Some(Candidate {
                        target: self.target,
                        source_generation: self.source_generation,
                        fields,
                        retained_bytes,
                        hidden_tab_count,
                        hidden_tab_metadata_bytes,
                        age_ticks: 0,
                        digest,
                        strong_capture: Arc::new(()),
                        reservation,
                    });
                }
            }

            fn begin_complete(&mut self, retained_bytes: u8, digest: u8) {
                self.begin(FieldSet::COMPLETE, retained_bytes, 2, 2, digest);
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
                    || !candidate.fields.is_complete()
                    || candidate.retained_bytes > self.retained_byte_budget
                    || candidate.hidden_tab_count > self.max_hidden_tabs
                    || candidate.hidden_tab_metadata_bytes > self.max_hidden_tab_metadata_bytes
                {
                    self.arena
                        .release(candidate.reservation)
                        .expect("a rejected candidate must release its reservation");
                    return CommitOutcome::Rejected;
                }
                if let Some(published) = &self.published {
                    if published.target == candidate.target
                        && published.source_generation == candidate.source_generation
                    {
                        if published.digest == candidate.digest {
                            self.arena
                                .release(candidate.reservation)
                                .expect("a no-op candidate must release its reservation");
                            return CommitOutcome::NoChange(published.publication_generation);
                        }
                        self.arena
                            .release(candidate.reservation)
                            .expect("an equivocating candidate must release its reservation");
                        return CommitOutcome::Rejected;
                    }
                }

                let Some(next) = self.publication_generation.checked_add(1) else {
                    self.arena
                        .release(candidate.reservation)
                        .expect("an overflowed candidate must release its reservation");
                    self.fail_exhausted();
                    return CommitOutcome::Exhausted;
                };
                if next == PUBLICATION_EXHAUSTED {
                    self.arena
                        .release(candidate.reservation)
                        .expect("an exhausted candidate must release its reservation");
                    self.fail_exhausted();
                    return CommitOutcome::Exhausted;
                }
                self.publication_generation = next;
                let published = Published {
                    target: candidate.target,
                    source_generation: candidate.source_generation,
                    publication_generation: next,
                    fields: candidate.fields,
                    retained_bytes: candidate.retained_bytes,
                    hidden_tab_count: candidate.hidden_tab_count,
                    hidden_tab_metadata_bytes: candidate.hidden_tab_metadata_bytes,
                    digest: candidate.digest,
                    reservation: candidate.reservation,
                };
                self.replace_published(Some(published));
                CommitOutcome::Published(next)
            }

            fn replace_published(&mut self, replacement: Option<Published>) {
                if let Some(previous) = self.published.take() {
                    let retained_by_render = self.rendering.as_ref().is_some_and(|rendering| {
                        rendering.reservation.token_id == previous.reservation.token_id
                    });
                    if !retained_by_render {
                        self.arena
                            .release(previous.reservation)
                            .expect("superseded publication must release its reservation");
                    }
                }
                self.published = replacement;
            }

            fn checked_generation_successor(current: u8) -> Option<u8> {
                current.checked_add(1).filter(|next| *next != u8::MAX)
            }

            fn advance(&mut self, domain: GenerationDomain) {
                let current = match domain {
                    GenerationDomain::Source => self.source_generation,
                    GenerationDomain::WindowIncarnation => self.target.window_incarnation,
                    GenerationDomain::WindowOrder => self.target.window_order_generation,
                    GenerationDomain::TabIncarnation => self.target.tab_incarnation,
                    GenerationDomain::PaneIncarnation => self.target.pane_incarnation,
                    GenerationDomain::Topology => self.target.topology_generation,
                    GenerationDomain::Geometry => self.target.geometry_generation,
                    GenerationDomain::Viewport => self.target.viewport_generation,
                    GenerationDomain::AlternateScreen => self.target.alternate_screen_generation,
                    GenerationDomain::Overlay => self.target.overlay_generation,
                    GenerationDomain::SelectionIme => self.target.selection_ime_generation,
                    GenerationDomain::Prediction => self.target.prediction_generation,
                    GenerationDomain::SynchronizedOutput => {
                        self.target.synchronized_output_generation
                    }
                    GenerationDomain::FontConfig => self.target.font_config_generation,
                    GenerationDomain::RendererCache => self.target.renderer_cache_generation,
                    GenerationDomain::Device => self.target.device_generation,
                    GenerationDomain::Damage => self.target.damage_generation,
                    GenerationDomain::RemoteConnection => self.target.remote_connection_incarnation,
                    GenerationDomain::RemoteDelivery => self.target.remote_delivery_generation,
                };
                let Some(next) = Self::checked_generation_successor(current) else {
                    self.fail_exhausted();
                    return;
                };
                match domain {
                    GenerationDomain::Source => self.source_generation = next,
                    GenerationDomain::WindowIncarnation => self.target.window_incarnation = next,
                    GenerationDomain::WindowOrder => self.target.window_order_generation = next,
                    GenerationDomain::TabIncarnation => self.target.tab_incarnation = next,
                    GenerationDomain::PaneIncarnation => self.target.pane_incarnation = next,
                    GenerationDomain::Topology => self.target.topology_generation = next,
                    GenerationDomain::Geometry => self.target.geometry_generation = next,
                    GenerationDomain::Viewport => self.target.viewport_generation = next,
                    GenerationDomain::AlternateScreen => {
                        self.target.alternate_screen_generation = next
                    }
                    GenerationDomain::Overlay => self.target.overlay_generation = next,
                    GenerationDomain::SelectionIme => self.target.selection_ime_generation = next,
                    GenerationDomain::Prediction => self.target.prediction_generation = next,
                    GenerationDomain::SynchronizedOutput => {
                        self.target.synchronized_output_generation = next
                    }
                    GenerationDomain::FontConfig => self.target.font_config_generation = next,
                    GenerationDomain::RendererCache => self.target.renderer_cache_generation = next,
                    GenerationDomain::Device => self.target.device_generation = next,
                    GenerationDomain::Damage => self.target.damage_generation = next,
                    GenerationDomain::RemoteConnection => {
                        self.target.remote_connection_incarnation = next
                    }
                    GenerationDomain::RemoteDelivery => {
                        self.target.remote_delivery_generation = next
                    }
                }
                if domain != GenerationDomain::Source {
                    self.replace_published(None);
                }
                if matches!(
                    domain,
                    GenerationDomain::WindowIncarnation
                        | GenerationDomain::TabIncarnation
                        | GenerationDomain::PaneIncarnation
                        | GenerationDomain::RemoteConnection
                ) {
                    self.source_generation = 0;
                }
            }

            fn force_generation(&mut self, domain: GenerationDomain, value: u8) {
                match domain {
                    GenerationDomain::Source => self.source_generation = value,
                    GenerationDomain::WindowIncarnation => self.target.window_incarnation = value,
                    GenerationDomain::WindowOrder => self.target.window_order_generation = value,
                    GenerationDomain::TabIncarnation => self.target.tab_incarnation = value,
                    GenerationDomain::PaneIncarnation => self.target.pane_incarnation = value,
                    GenerationDomain::Topology => self.target.topology_generation = value,
                    GenerationDomain::Geometry => self.target.geometry_generation = value,
                    GenerationDomain::Viewport => self.target.viewport_generation = value,
                    GenerationDomain::AlternateScreen => {
                        self.target.alternate_screen_generation = value
                    }
                    GenerationDomain::Overlay => self.target.overlay_generation = value,
                    GenerationDomain::SelectionIme => self.target.selection_ime_generation = value,
                    GenerationDomain::Prediction => self.target.prediction_generation = value,
                    GenerationDomain::SynchronizedOutput => {
                        self.target.synchronized_output_generation = value
                    }
                    GenerationDomain::FontConfig => self.target.font_config_generation = value,
                    GenerationDomain::RendererCache => {
                        self.target.renderer_cache_generation = value
                    }
                    GenerationDomain::Device => self.target.device_generation = value,
                    GenerationDomain::Damage => self.target.damage_generation = value,
                    GenerationDomain::RemoteConnection => {
                        self.target.remote_connection_incarnation = value
                    }
                    GenerationDomain::RemoteDelivery => {
                        self.target.remote_delivery_generation = value
                    }
                }
            }

            fn tick(&mut self) {
                let Some(candidate) = self.pending.as_mut() else {
                    return;
                };
                let Some(next_age) = candidate.age_ticks.checked_add(1) else {
                    self.drop_candidate();
                    return;
                };
                if next_age >= self.candidate_lifetime_ticks {
                    self.drop_candidate();
                } else {
                    candidate.age_ticks = next_age;
                }
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
                self.target = ExactTarget {
                    session: next_session,
                    ..ExactTarget::initial()
                };
                self.source_generation = 0;
                self.publication_generation = 0;
                self.attached = true;
                self.exhausted = false;
                self.drop_candidate();
                self.replace_published(None);
            }

            fn detach(&mut self) {
                self.attached = false;
                self.drop_candidate();
                self.replace_published(None);
            }

            fn acquire_render(&mut self) {
                if self.rendering.is_none() {
                    self.rendering = self.published.clone();
                }
            }

            fn release_render(&mut self) -> bool {
                let Some(rendering) = self.rendering.take() else {
                    return false;
                };
                let settles = !self.exhausted
                    && self.attached
                    && rendering.target == self.target
                    && self.published.as_ref().is_some_and(|published| {
                        published.target == rendering.target
                            && published.publication_generation == rendering.publication_generation
                    });
                let retained_by_publication = self.published.as_ref().is_some_and(|published| {
                    published.reservation.token_id == rendering.reservation.token_id
                });
                if !retained_by_publication {
                    self.arena
                        .release(rendering.reservation)
                        .expect("a settled render generation must release its reservation");
                }
                settles
            }

            fn apply(&mut self, event: Event) -> Result<(), String> {
                let prior_publication_generation = self.publication_generation;
                let prior_published = self.published.clone();
                let mut commit_outcome = None;
                match event {
                    Event::BeginValid => self.begin_complete(4, 1),
                    Event::BeginIncomplete => self.begin(
                        FieldSet::COMPLETE.without(RequiredField::Cursor),
                        4,
                        2,
                        2,
                        2,
                    ),
                    Event::BeginUnresolvedImage => self.begin(
                        FieldSet::COMPLETE.without(RequiredField::HyperlinksAndImages),
                        4,
                        2,
                        2,
                        3,
                    ),
                    Event::BeginOverBudget => self.begin_complete(9, 4),
                    Event::BeginTooManyTabs => self.begin(FieldSet::COMPLETE, 4, 4, 2, 5),
                    Event::BeginTabMetadataOverBudget => self.begin(FieldSet::COMPLETE, 4, 2, 5, 6),
                    Event::Commit => {
                        commit_outcome = Some(self.commit());
                    }
                    Event::CancelBeforePublication => self.drop_candidate(),
                    Event::CancelAfterPublication => {}
                    Event::Tick => self.tick(),
                    Event::AcquireRender => self.acquire_render(),
                    Event::ReleaseRender => {
                        let _ = self.release_render();
                    }
                    Event::Advance(domain) => self.advance(domain),
                    Event::ImageHyperlinkMutation => self.advance(GenerationDomain::Source),
                    Event::Reconnect => self.reconnect(),
                    Event::Detach => self.detach(),
                }
                self.check_invariants()?;
                if matches!(
                    event,
                    Event::CancelBeforePublication | Event::CancelAfterPublication | Event::Tick
                ) || matches!(
                    commit_outcome,
                    Some(CommitOutcome::Rejected | CommitOutcome::NoChange(_))
                ) {
                    if self.publication_generation != prior_publication_generation
                        || self.published != prior_published
                    {
                        return Err(format!(
                            "{event:?} mutated publication despite cancellation/rejection semantics"
                        ));
                    }
                }
                Ok(())
            }

            fn reservation_matches(
                &self,
                reservation: &WindowReservation,
                retained_bytes: u8,
                hidden_tabs: u8,
                hidden_tab_metadata_bytes: u8,
            ) -> bool {
                reservation.arena_id == self.arena.arena_id
                    && reservation.publisher_id == self.publisher_id
                    && reservation.retained_bytes == usize::from(retained_bytes)
                    && reservation.hidden_tabs == usize::from(hidden_tabs)
                    && reservation.hidden_tab_metadata_bytes
                        == usize::from(hidden_tab_metadata_bytes)
            }

            fn check_invariants(&self) -> Result<(), String> {
                if self.publication_generation >= PUBLICATION_EXHAUSTED {
                    return Err("publication generation crossed the terminal sentinel".into());
                }
                let retained_slots = usize::from(self.pending.is_some())
                    + usize::from(self.published.is_some())
                    + usize::from(self.rendering.is_some());
                if retained_slots > 3 {
                    return Err(format!(
                        "retained generation slots exceeded: {retained_slots}"
                    ));
                }
                self.arena.check_invariants()?;
                let mut exact_tokens = BTreeSet::new();
                if let Some(candidate) = &self.pending {
                    exact_tokens.insert(candidate.reservation.token_id);
                }
                if let Some(published) = &self.published {
                    exact_tokens.insert(published.reservation.token_id);
                }
                if let Some(rendering) = &self.rendering {
                    exact_tokens.insert(rendering.reservation.token_id);
                }
                if exact_tokens != self.arena.live.keys().copied().collect::<BTreeSet<_>>() {
                    return Err("retained generations and exact arena tokens diverged".into());
                }
                let retained_bytes = self.retained_generation_bytes();
                if retained_bytes > usize::from(self.retained_byte_budget) {
                    return Err(format!(
                        "retained bytes {retained_bytes} exceeded budget {}",
                        self.retained_byte_budget
                    ));
                }
                if retained_bytes != self.arena.retained_bytes {
                    return Err("retained-generation bytes diverged from the arena ledger".into());
                }
                if (self.exhausted || !self.attached) && self.published.is_some() {
                    return Err("detached or exhausted publisher retained eligibility".into());
                }
                if let Some(published) = &self.published {
                    if published.target != self.target {
                        return Err("published target is stale".into());
                    }
                    if published.source_generation > self.source_generation {
                        return Err("published source is from the future".into());
                    }
                    if published.publication_generation == 0 || !published.fields.is_complete() {
                        return Err("published snapshot is unstamped or incomplete".into());
                    }
                    if published.hidden_tab_count > self.max_hidden_tabs
                        || published.hidden_tab_metadata_bytes > self.max_hidden_tab_metadata_bytes
                    {
                        return Err("published hidden-tab accounting exceeded its cap".into());
                    }
                    if !self.reservation_matches(
                        &published.reservation,
                        published.retained_bytes,
                        published.hidden_tab_count,
                        published.hidden_tab_metadata_bytes,
                    ) {
                        return Err("published generation has mismatched arena accounting".into());
                    }
                }
                if let Some(rendering) = &self.rendering {
                    if rendering.publication_generation == 0 || !rendering.fields.is_complete() {
                        return Err("render lease is unstamped or incomplete".into());
                    }
                    if rendering.hidden_tab_count > self.max_hidden_tabs
                        || rendering.hidden_tab_metadata_bytes > self.max_hidden_tab_metadata_bytes
                    {
                        return Err("render hidden-tab accounting exceeded its cap".into());
                    }
                    if !self.reservation_matches(
                        &rendering.reservation,
                        rendering.retained_bytes,
                        rendering.hidden_tab_count,
                        rendering.hidden_tab_metadata_bytes,
                    ) {
                        return Err("render generation has mismatched arena accounting".into());
                    }
                }
                if let Some(pending) = &self.pending {
                    if pending.age_ticks >= self.candidate_lifetime_ticks {
                        return Err("candidate exceeded its bounded lifetime".into());
                    }
                    if pending.hidden_tab_count > self.max_hidden_tabs
                        || pending.hidden_tab_metadata_bytes > self.max_hidden_tab_metadata_bytes
                    {
                        return Err("candidate hidden-tab accounting exceeded its cap".into());
                    }
                    if !self.reservation_matches(
                        &pending.reservation,
                        pending.retained_bytes,
                        pending.hidden_tab_count,
                        pending.hidden_tab_metadata_bytes,
                    ) {
                        return Err("candidate has mismatched arena accounting".into());
                    }
                }
                Ok(())
            }

            fn assert_invariants(&self) {
                if let Err(error) = self.check_invariants() {
                    panic!("publication-model invariant failed: {}", error);
                }
            }
        }

        #[derive(Clone, Debug, Eq, PartialEq)]
        struct WindowReservation {
            arena_id: u16,
            publisher_id: u16,
            token_id: u16,
            retained_bytes: usize,
            hidden_tabs: usize,
            hidden_tab_metadata_bytes: usize,
        }

        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        struct ReservationAccounting {
            publisher_id: u16,
            retained_bytes: usize,
            hidden_tabs: usize,
            hidden_tab_metadata_bytes: usize,
        }

        #[derive(Clone, Debug, Eq, PartialEq)]
        struct SessionSnapshotBudget {
            arena_id: u16,
            max_publishers: usize,
            max_retained_bytes: usize,
            max_hidden_tabs: usize,
            max_hidden_tab_metadata_bytes: usize,
            next_publisher_id: u16,
            next_token_id: u16,
            publishers: BTreeSet<u16>,
            live: BTreeMap<u16, ReservationAccounting>,
            retained_bytes: usize,
            hidden_tabs: usize,
            hidden_tab_metadata_bytes: usize,
        }

        impl SessionSnapshotBudget {
            fn new(
                max_publishers: usize,
                max_retained_bytes: usize,
                max_hidden_tabs: usize,
                max_hidden_tab_metadata_bytes: usize,
                arena_id: u16,
            ) -> Self {
                Self {
                    arena_id,
                    max_publishers,
                    max_retained_bytes,
                    max_hidden_tabs,
                    max_hidden_tab_metadata_bytes,
                    next_publisher_id: 1,
                    next_token_id: 1,
                    publishers: BTreeSet::new(),
                    live: BTreeMap::new(),
                    retained_bytes: 0,
                    hidden_tabs: 0,
                    hidden_tab_metadata_bytes: 0,
                }
            }

            fn register_publisher(&mut self) -> Option<u16> {
                if self.publishers.len() >= self.max_publishers {
                    return None;
                }
                let publisher_id = self.next_publisher_id;
                self.next_publisher_id = self.next_publisher_id.checked_add(1)?;
                if !self.publishers.insert(publisher_id) {
                    return None;
                }
                Some(publisher_id)
            }

            fn try_admit(
                &mut self,
                publisher_id: u16,
                retained_bytes: usize,
                hidden_tabs: usize,
                hidden_tab_metadata_bytes: usize,
            ) -> Option<WindowReservation> {
                if !self.publishers.contains(&publisher_id) {
                    return None;
                }
                let aggregate_bytes = self.retained_bytes.checked_add(retained_bytes)?;
                let aggregate_tabs = self.hidden_tabs.checked_add(hidden_tabs)?;
                let aggregate_metadata = self
                    .hidden_tab_metadata_bytes
                    .checked_add(hidden_tab_metadata_bytes)?;
                if aggregate_bytes > self.max_retained_bytes
                    || aggregate_tabs > self.max_hidden_tabs
                    || aggregate_metadata > self.max_hidden_tab_metadata_bytes
                {
                    return None;
                }
                let token_id = self.next_token_id;
                self.next_token_id = self.next_token_id.checked_add(1)?;
                let accounting = ReservationAccounting {
                    publisher_id,
                    retained_bytes,
                    hidden_tabs,
                    hidden_tab_metadata_bytes,
                };
                if self.live.contains_key(&token_id) {
                    return None;
                }
                self.live.insert(token_id, accounting);
                self.retained_bytes = aggregate_bytes;
                self.hidden_tabs = aggregate_tabs;
                self.hidden_tab_metadata_bytes = aggregate_metadata;
                Some(WindowReservation {
                    arena_id: self.arena_id,
                    publisher_id,
                    token_id,
                    retained_bytes,
                    hidden_tabs,
                    hidden_tab_metadata_bytes,
                })
            }

            fn release(&mut self, reservation: WindowReservation) -> Result<(), &'static str> {
                if reservation.arena_id != self.arena_id {
                    return Err("reservation belongs to another arena");
                }
                let expected = ReservationAccounting {
                    publisher_id: reservation.publisher_id,
                    retained_bytes: reservation.retained_bytes,
                    hidden_tabs: reservation.hidden_tabs,
                    hidden_tab_metadata_bytes: reservation.hidden_tab_metadata_bytes,
                };
                if self.live.get(&reservation.token_id) != Some(&expected) {
                    return Err("reservation is stale, duplicated, or has mismatched accounting");
                }
                let retained_bytes = self
                    .retained_bytes
                    .checked_sub(reservation.retained_bytes)
                    .ok_or("arena retained-byte accounting underflow")?;
                let hidden_tabs = self
                    .hidden_tabs
                    .checked_sub(reservation.hidden_tabs)
                    .ok_or("arena hidden-tab accounting underflow")?;
                let hidden_tab_metadata_bytes = self
                    .hidden_tab_metadata_bytes
                    .checked_sub(reservation.hidden_tab_metadata_bytes)
                    .ok_or("arena metadata accounting underflow")?;
                self.retained_bytes = retained_bytes;
                self.hidden_tabs = hidden_tabs;
                self.hidden_tab_metadata_bytes = hidden_tab_metadata_bytes;
                self.live.remove(&reservation.token_id);
                Ok(())
            }

            fn check_invariants(&self) -> Result<(), String> {
                let retained_bytes = self.live.values().try_fold(0_usize, |total, item| {
                    total.checked_add(item.retained_bytes)
                });
                let hidden_tabs = self
                    .live
                    .values()
                    .try_fold(0_usize, |total, item| total.checked_add(item.hidden_tabs));
                let metadata = self.live.values().try_fold(0_usize, |total, item| {
                    total.checked_add(item.hidden_tab_metadata_bytes)
                });
                if retained_bytes != Some(self.retained_bytes)
                    || hidden_tabs != Some(self.hidden_tabs)
                    || metadata != Some(self.hidden_tab_metadata_bytes)
                {
                    return Err("arena totals diverged from its exact live-token ledger".into());
                }
                if self.publishers.len() > self.max_publishers
                    || self.retained_bytes > self.max_retained_bytes
                    || self.hidden_tabs > self.max_hidden_tabs
                    || self.hidden_tab_metadata_bytes > self.max_hidden_tab_metadata_bytes
                {
                    return Err("arena exceeded a configured aggregate cap".into());
                }
                if self
                    .live
                    .values()
                    .any(|item| !self.publishers.contains(&item.publisher_id))
                {
                    return Err("arena retained a token for an unknown publisher".into());
                }
                Ok(())
            }
        }

        #[test]
        fn last_known_good_survives_content_build_failure_but_not_geometry_or_identity_change() {
            let mut model = PublicationModel::new(8);
            model.begin_complete(4, 1);
            assert_eq!(model.commit(), CommitOutcome::Published(1));
            let first = model.published.clone();

            model.advance(GenerationDomain::Source);
            model.begin(
                FieldSet::COMPLETE.without(RequiredField::Cursor),
                4,
                2,
                2,
                2,
            );
            assert_eq!(model.commit(), CommitOutcome::Rejected);
            assert_eq!(model.published, first);

            model.advance(GenerationDomain::Geometry);
            assert!(model.published.is_none());
            model.begin_complete(4, 3);
            model.advance(GenerationDomain::PaneIncarnation);
            assert_eq!(model.commit(), CommitOutcome::Rejected);
            assert!(model.published.is_none());
        }

        #[test]
        fn stale_incomplete_over_budget_and_same_source_equivocation_fail_closed() {
            let mut model = PublicationModel::new(8);
            model.begin(
                FieldSet::COMPLETE.without(RequiredField::TerminalMetadata),
                4,
                2,
                2,
                1,
            );
            assert_eq!(model.commit(), CommitOutcome::Rejected);
            model.begin_complete(9, 1);
            assert_eq!(model.commit(), CommitOutcome::Rejected);

            model.begin_complete(4, 1);
            assert_eq!(model.commit(), CommitOutcome::Published(1));
            model.begin_complete(4, 1);
            assert_eq!(model.commit(), CommitOutcome::NoChange(1));
            model.begin_complete(4, 2);
            assert_eq!(model.commit(), CommitOutcome::Rejected);

            model.advance(GenerationDomain::Source);
            model.begin_complete(4, 3);
            model.advance(GenerationDomain::Source);
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
                model.begin_complete(4, next_publication);
                assert_eq!(model.commit(), CommitOutcome::Published(next_publication));
            }
            model.source_generation = PUBLICATION_EXHAUSTED - 1;
            model.begin_complete(4, PUBLICATION_EXHAUSTED);
            assert_eq!(model.commit(), CommitOutcome::Exhausted);
            assert!(model.exhausted);
            assert!(model.published.is_none());
            assert_eq!(model.commit(), CommitOutcome::Exhausted);

            model.reconnect();
            model.begin_complete(4, 1);
            assert_eq!(model.commit(), CommitOutcome::Published(1));
        }

        #[test]
        fn every_named_render_state_class_has_explicit_stale_publication_semantics() {
            let mut model = PublicationModel::new(8);
            model.begin_complete(4, 1);
            assert_eq!(model.commit(), CommitOutcome::Published(1));

            let last_known_good = model.published.clone();
            model
                .apply(Event::ImageHyperlinkMutation)
                .expect("image mutation must preserve invariants");
            assert_eq!(model.published, last_known_good);
            model.begin_complete(4, 2);
            model
                .apply(Event::Advance(GenerationDomain::AlternateScreen))
                .expect("alternate-screen mutation must preserve invariants");
            assert_eq!(model.commit(), CommitOutcome::Rejected);
            assert!(model.published.is_none());

            model.begin_complete(4, 3);
            assert_eq!(model.commit(), CommitOutcome::Published(2));
            model.begin_complete(4, 4);
            model
                .apply(Event::Advance(GenerationDomain::SelectionIme))
                .expect("selection/IME mutation must preserve invariants");
            assert_eq!(model.commit(), CommitOutcome::Rejected);
            assert!(model.published.is_none());

            model.begin_complete(4, 5);
            assert_eq!(model.commit(), CommitOutcome::Published(3));
            model.begin_complete(4, 6);
            model
                .apply(Event::Advance(GenerationDomain::Topology))
                .expect("topology mutation must preserve invariants");
            assert_eq!(model.commit(), CommitOutcome::Rejected);
            assert!(model.published.is_none());
        }

        #[test]
        fn every_generation_domain_fails_closed_before_its_exhausted_sentinel() {
            for domain in GENERATION_DOMAINS {
                let mut model = PublicationModel::new(8);
                model.begin_complete(4, 1);
                assert_eq!(model.commit(), CommitOutcome::Published(1));
                model.force_generation(domain, u8::MAX - 1);
                model
                    .apply(Event::Advance(domain))
                    .unwrap_or_else(|error| panic!("{:?} violated invariants: {}", domain, error));
                assert!(model.exhausted);
                assert!(model.pending.is_none());
                assert!(model.published.is_none());
            }

            let mut session = PublicationModel::new(8);
            session.begin_complete(4, 1);
            assert_eq!(session.commit(), CommitOutcome::Published(1));
            session.target.session = u8::MAX - 1;
            session
                .apply(Event::Reconnect)
                .expect("session exhaustion must preserve invariants");
            assert!(session.exhausted);
            assert!(session.published.is_none());
        }

        #[test]
        fn newer_publication_supersedes_an_in_flight_frame_without_unbounded_generations() {
            let mut model = PublicationModel::new(8);
            model.begin_complete(4, 1);
            assert_eq!(model.commit(), CommitOutcome::Published(1));
            model.acquire_render();
            let first_render = model.rendering.clone();
            model.acquire_render();
            assert_eq!(model.rendering, first_render);

            model.advance(GenerationDomain::Source);
            model.begin_complete(4, 2);
            assert_eq!(model.commit(), CommitOutcome::Published(2));
            model.advance(GenerationDomain::Source);
            model.begin_complete(4, 3);
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
        fn every_required_field_is_auditable_and_missing_fields_reject_publication() {
            assert_eq!(FieldSet::COMPLETE.missing(), Vec::<RequiredField>::new());
            for field in REQUIRED_FIELDS {
                let fields = FieldSet::COMPLETE.without(field);
                assert_eq!(fields.missing(), vec![field]);
                let mut model = PublicationModel::new(8);
                model.begin(fields, 4, 2, 2, 1);
                assert_eq!(model.commit(), CommitOutcome::Rejected, "{field:?}");
                assert!(model.published.is_none(), "{field:?}");
            }
        }

        #[test]
        fn cancellation_deadline_detach_and_stale_settlement_release_exact_candidates() {
            let mut model = PublicationModel::new(8);
            model.begin_complete(4, 1);
            let cancelled = Arc::downgrade(&model.pending.as_ref().unwrap().strong_capture);
            let before_cancel = (model.publication_generation, model.published.clone());
            model
                .apply(Event::CancelBeforePublication)
                .expect("pre-publication cancellation must preserve invariants");
            assert!(cancelled.upgrade().is_none());
            assert_eq!(
                (model.publication_generation, model.published),
                before_cancel
            );

            model.begin_complete(4, 2);
            assert_eq!(model.commit(), CommitOutcome::Published(1));
            let last_known_good = model.published.clone();
            let publication_generation = model.publication_generation;
            model.advance(GenerationDomain::Source);
            model.begin_complete(4, 3);
            let expired = Arc::downgrade(&model.pending.as_ref().unwrap().strong_capture);
            for _ in 0..CANDIDATE_LIFETIME_TICKS {
                model
                    .apply(Event::Tick)
                    .expect("candidate deadline must preserve invariants");
            }
            assert!(model.pending.is_none());
            assert!(expired.upgrade().is_none());
            assert_eq!(model.publication_generation, publication_generation);
            assert_eq!(model.published, last_known_good);

            let published = model.published.clone();
            model
                .apply(Event::CancelAfterPublication)
                .expect("post-publication cancellation must not retract the snapshot");
            assert_eq!(model.published, published);

            model.acquire_render();
            model.detach();
            assert!(model.published.is_none());
            assert!(!model.release_render());
            model.assert_invariants();
        }

        #[test]
        fn per_window_and_session_budgets_bound_hidden_tabs_metadata_and_publishers() {
            let mut window = PublicationModel::new(8);
            window.begin(FieldSet::COMPLETE, 4, 4, 2, 1);
            assert!(window.pending.is_none());
            window.begin(FieldSet::COMPLETE, 4, 2, 5, 2);
            assert!(window.pending.is_none());

            let mut session = SessionSnapshotBudget::new(2, 12, 5, 7, 41);
            let first_publisher = session.register_publisher().expect("first publisher");
            let second_publisher = session.register_publisher().expect("second publisher");
            assert!(session.register_publisher().is_none());
            let first = session
                .try_admit(first_publisher, 4, 2, 3)
                .expect("first reservation");
            let duplicate = first.clone();
            let cross_arena = first.clone();
            let second = session
                .try_admit(second_publisher, 6, 3, 4)
                .expect("second reservation");
            let full = session.clone();
            assert!(session.try_admit(first_publisher, 1, 1, 0).is_none());
            assert_eq!(session, full);
            session.release(first).expect("first release");
            assert!(session.release(duplicate).is_err());
            let mut foreign = SessionSnapshotBudget::new(1, 12, 5, 7, 42);
            let _ = foreign.register_publisher().expect("foreign publisher");
            assert!(foreign.release(cross_arena).is_err());
            let replacement = session
                .try_admit(first_publisher, 4, 2, 3)
                .expect("released aggregate capacity must be reusable");
            session.release(second).expect("second release");
            session.release(replacement).expect("replacement release");
            session
                .check_invariants()
                .expect("session arena invariants");
            assert_eq!(session.publishers.len(), 2);
            assert_eq!(session.retained_bytes, 0);
            assert_eq!(session.hidden_tabs, 0);
            assert_eq!(session.hidden_tab_metadata_bytes, 0);

            let mut lifecycle = PublicationModel::new(12);
            lifecycle.arena.max_hidden_tabs = 5;
            lifecycle.arena.max_hidden_tab_metadata_bytes = 5;
            lifecycle.begin_complete(4, 1);
            assert_eq!(lifecycle.commit(), CommitOutcome::Published(1));
            lifecycle.acquire_render();
            lifecycle.advance(GenerationDomain::Source);
            lifecycle.begin_complete(4, 2);
            assert_eq!(lifecycle.commit(), CommitOutcome::Published(2));
            lifecycle.advance(GenerationDomain::Source);
            lifecycle.begin_complete(4, 3);
            assert!(lifecycle.pending.is_none());
            assert_eq!(lifecycle.arena.hidden_tabs, 4);
            assert_eq!(lifecycle.arena.hidden_tab_metadata_bytes, 4);
            lifecycle.assert_invariants();
        }

        #[test]
        fn thread_scheduled_publication_exposes_only_whole_immutable_snapshots() {
            let mut first_model = PublicationModel::new(8);
            first_model.begin_complete(4, 11);
            assert_eq!(first_model.commit(), CommitOutcome::Published(1));
            let first = Arc::new(first_model.published.unwrap());

            let mut second_model = PublicationModel::new(8);
            second_model.advance(GenerationDomain::Source);
            second_model.begin_complete(4, 22);
            assert_eq!(second_model.commit(), CommitOutcome::Published(1));
            let second = Arc::new(second_model.published.unwrap());

            let slot = Arc::new(Mutex::new(Arc::clone(&first)));
            let start = Arc::new(Barrier::new(2));
            std::thread::scope(|scope| {
                let writer_slot = Arc::clone(&slot);
                let writer_start = Arc::clone(&start);
                let writer_first = Arc::clone(&first);
                let writer_second = Arc::clone(&second);
                scope.spawn(move || {
                    writer_start.wait();
                    for iteration in 0..1_024 {
                        let replacement = if iteration % 2 == 0 {
                            Arc::clone(&writer_first)
                        } else {
                            Arc::clone(&writer_second)
                        };
                        *writer_slot.lock().expect("publication slot poisoned") = replacement;
                        std::thread::yield_now();
                    }
                });

                start.wait();
                for _ in 0..1_024 {
                    let observed = Arc::clone(&slot.lock().expect("publication slot poisoned"));
                    assert!(observed.fields.is_complete());
                    assert!(
                        (*observed == *first && observed.digest == 11)
                            || (*observed == *second && observed.digest == 22)
                    );
                    std::thread::yield_now();
                }
            });
        }

        #[test]
        fn bounded_event_interleavings_preserve_publication_invariants() {
            fn visit(model: PublicationModel, depth: usize, trace: &mut Vec<Event>) {
                if depth == 0 {
                    return;
                }
                for event in EVENTS {
                    let mut next = model.clone();
                    trace.push(event);
                    if let Err(error) = next.apply(event) {
                        panic!("counterexample trace {:?}: {}", trace, error);
                    }
                    visit(next, depth - 1, trace);
                    trace.pop();
                }
            }

            visit(PublicationModel::new(8), 4, &mut Vec::new());
        }
    }
}
