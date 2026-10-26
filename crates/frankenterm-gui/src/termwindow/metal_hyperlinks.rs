//! Hit targets from the immutable Metal frame that actually reached the display.
//!
//! Row spans are built from the resolved render mirror after its terminal lock
//! is released. Drawable callbacks publish one bounded latest snapshot; mouse
//! motion only reads those owned spans and checks pane/geometry identity.

use crate::selection::{FrameHyperlinks, HyperlinkSpan, retain_hyperlink_span};
use finl_unicode::grapheme_clusters::Graphemes;
use frankenterm_renderer_metal::{
    BackgroundUniforms, FramePresentation, FramePresentationHandler, PixelRect,
};
use mux::pane::Pane;
use mux::render_mirror::{MirrorLineMode, MirrorRow, RenderMirror};
use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::{Arc, Mutex, PoisonError};
use termwiz::cell::{Cell, CellAttributes};
use termwiz::hyperlink::Hyperlink;
use wezterm_term::StableRowIndex;

type Geometry = [usize; 12];

/// The window witness is compared with the mouse event's current pure layout
/// witness. The viewport and pixel-to-row mapping belong to this exact draw.
#[derive(Debug)]
pub(crate) struct PaneHyperlinks {
    links: FrameHyperlinks,
    geometry: Geometry,
    first: StableRowIndex,
    rows: usize,
    origin: [f32; 2],
    cell_height: f32,
    clip: PixelRect,
}

impl PaneHyperlinks {
    fn link_at(
        &self,
        pane: &Arc<dyn Pane>,
        geometry: Geometry,
        pixel: [f32; 2],
    ) -> Option<Arc<Hyperlink>> {
        if self.geometry != geometry || pixel.iter().any(|value| !value.is_finite()) {
            return None;
        }
        let [x, y] = pixel;
        if x < self.clip.x as f32
            || y < self.clip.y as f32
            || x >= self.clip.x.saturating_add(self.clip.width) as f32
            || y >= self.clip.y.saturating_add(self.clip.height) as f32
        {
            return None;
        }
        let row = ((y - self.origin[1]) / self.cell_height).floor();
        if row < 0.0 || row >= self.rows as f32 {
            return None;
        }
        let stable = self.first.checked_add(row as StableRowIndex)?;
        self.links.link_at(pane, stable, x)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RowKey {
    generation: u64,
    mode: MirrorLineMode,
    composed: Range<usize>,
}

struct CachedRow {
    key: RowKey,
    spans: Arc<[HyperlinkSpan]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Layout {
    geometry: Geometry,
    origin: [u32; 2],
    cell_size: [u32; 2],
    clip: PixelRect,
    cols: usize,
}

/// Only currently visible rows are retained. A scroll reuses rows by stable
/// index; output, a line-mode change or composition rebuilds its affected rows.
#[derive(Default)]
pub(crate) struct HyperlinkRowCache {
    rows: BTreeMap<StableRowIndex, CachedRow>,
    layout: Option<Layout>,
    snapshot: Option<Arc<PaneHyperlinks>>,
    #[cfg(test)]
    rebuilt: usize,
}

impl HyperlinkRowCache {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn update(
        &mut self,
        pane: &Arc<dyn Pane>,
        mirror: &RenderMirror,
        geometry: Option<Geometry>,
        clip: PixelRect,
        background: &BackgroundUniforms,
        composition: Option<&str>,
    ) -> Option<Arc<PaneHyperlinks>> {
        #[cfg(test)]
        {
            self.rebuilt = 0;
        }
        let geometry = geometry?;
        let dimensions = mirror.dimensions()?;
        if background
            .cell_size
            .iter()
            .any(|value| !value.is_finite() || *value <= 0.0)
            || background
                .grid_origin
                .iter()
                .any(|value| !value.is_finite())
        {
            return None;
        }
        let layout = Layout {
            geometry,
            origin: background.grid_origin.map(f32::to_bits),
            cell_size: background.cell_size.map(f32::to_bits),
            clip,
            cols: dimensions.cols,
        };
        let same_owner = self
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.links.belongs_to(pane));
        let mut changed = !same_owner || self.layout != Some(layout);
        if changed {
            self.rows.clear();
        }
        let first = mirror.first();
        let end = first.checked_add(StableRowIndex::try_from(mirror.rows().len()).ok()?)?;
        self.rows.retain(|row, _| (first..end).contains(row));
        let cursor = mirror.cursor();
        // Line::overlay_text_with_attribute writes each grapheme as a Cell.
        // Even a zero-column grapheme replaces at least one physical cell.
        let width = composition.map_or(0, |text| {
            Graphemes::new(text).fold(0_usize, |width, text| {
                width.saturating_add(
                    Cell::new_grapheme(text, CellAttributes::blank(), None)
                        .width()
                        .max(1),
                )
            })
        });
        for row in mirror.rows() {
            let composed = if width > 0 && row.stable() == cursor.y {
                cursor.x..cursor.x.saturating_add(width)
            } else {
                0..0
            };
            let key = RowKey {
                generation: row.generation(),
                mode: row.line_mode(),
                composed,
            };
            if self
                .rows
                .get(&row.stable())
                .is_none_or(|cached| cached.key != key)
            {
                let spans = row_spans(row, &key.composed, &layout);
                self.rows.insert(
                    row.stable(),
                    CachedRow {
                        key,
                        spans: spans.into(),
                    },
                );
                changed = true;
                #[cfg(test)]
                {
                    self.rebuilt += 1;
                }
            }
        }
        changed |= self
            .snapshot
            .as_ref()
            .is_none_or(|snapshot| snapshot.first != first || snapshot.rows != mirror.rows().len());
        if changed {
            let mut links = FrameHyperlinks::new(pane);
            for row in mirror.rows() {
                links.retain_shared_line(row.stable(), Arc::clone(&self.rows[&row.stable()].spans));
            }
            self.snapshot = Some(Arc::new(PaneHyperlinks {
                links,
                geometry,
                first,
                rows: mirror.rows().len(),
                origin: background.grid_origin,
                cell_height: background.cell_size[1],
                clip,
            }));
        }
        self.layout = Some(layout);
        self.snapshot.as_ref().map(Arc::clone)
    }
}

fn row_spans(row: &MirrorRow, composed: &Range<usize>, layout: &Layout) -> Vec<HyperlinkSpan> {
    let origin = f32::from_bits(layout.origin[0]);
    let cell_width = f32::from_bits(layout.cell_size[0]);
    let width = cell_width
        * if row.line_mode() == MirrorLineMode::SingleWidth {
            1.0
        } else {
            2.0
        };
    let clip = origin.max(layout.clip.x as f32)
        ..(origin + layout.cols as f32 * cell_width)
            .min(layout.clip.x.saturating_add(layout.clip.width) as f32);
    let mut spans = Vec::new();
    for cell in row.cells() {
        let Some(link) = row.hyperlink(cell) else {
            continue;
        };
        let start = cell.col();
        let end = start.saturating_add(cell.width().max(1));
        let mut retain = |range: Range<usize>| {
            if range.start < range.end {
                retain_hyperlink_span(
                    &mut spans,
                    Some(link),
                    origin + range.start as f32 * width..origin + range.end as f32 * width,
                    clip.clone(),
                );
            }
        };
        if composed.is_empty() {
            retain(start..end);
        } else {
            retain(start..end.min(composed.start));
            retain(start.max(composed.end)..end);
        }
    }
    spans
}

struct PresentedFrame {
    frame: u64,
    size: [u32; 2],
    panes: Arc<[Arc<PaneHyperlinks>]>,
}

#[derive(Default)]
struct PresentedState {
    epoch: u64,
    latest: Option<PresentedFrame>,
}

/// Shared by the rendering owner and main-thread hit testing. There is no
/// pending-frame queue: each native handler owns its immutable snapshot, and
/// only the newest successful callback replaces this single presented frame.
#[derive(Default)]
pub(crate) struct MetalHyperlinks {
    state: Mutex<PresentedState>,
}

impl MetalHyperlinks {
    pub(crate) fn begin_epoch(&self) -> u64 {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.epoch = state
            .epoch
            .checked_add(1)
            .expect("Metal hyperlink epoch exhausted");
        state.latest = None;
        state.epoch
    }

    pub(crate) fn retire_epoch(&self, epoch: u64) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.epoch == epoch {
            state.epoch = state
                .epoch
                .checked_add(1)
                .expect("Metal hyperlink epoch exhausted");
            state.latest = None;
        }
    }

    pub(crate) fn handler(
        self: &Arc<Self>,
        epoch: u64,
        panes: Arc<[Arc<PaneHyperlinks>]>,
        observer: Option<FramePresentationHandler>,
    ) -> FramePresentationHandler {
        let state = Arc::downgrade(self);
        Arc::new(move |event| {
            if event.presented_at.is_some() {
                if let Some(state) = state.upgrade() {
                    state.present(epoch, event, &panes);
                }
            }
            if let Some(observer) = &observer {
                observer(event);
            }
        })
    }

    fn present(&self, epoch: u64, event: FramePresentation, panes: &Arc<[Arc<PaneHyperlinks>]>) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.epoch != epoch
            || state
                .latest
                .as_ref()
                .is_some_and(|latest| event.frame <= latest.frame)
        {
            return;
        }
        state.latest = Some(PresentedFrame {
            frame: event.frame,
            size: event.size,
            panes: Arc::clone(panes),
        });
    }

    pub(crate) fn link_at(
        &self,
        pane: &Arc<dyn Pane>,
        geometry: Geometry,
        pixel: [f32; 2],
    ) -> Option<Arc<Hyperlink>> {
        let panes = {
            let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            let latest = state.latest.as_ref()?;
            if [
                usize::try_from(latest.size[0]).ok()?,
                usize::try_from(latest.size[1]).ok()?,
            ] != [geometry[6], geometry[7]]
            {
                return None;
            }
            Arc::clone(&latest.panes)
        };
        panes
            .iter()
            .find_map(|snapshot| snapshot.link_at(pane, geometry, pixel))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use mux::render_mirror::{CaptureRequest, capture_terminal_rows};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;
    use wezterm_term::{Terminal, TerminalConfiguration, TerminalSize};

    const GEOMETRY: Geometry = [8, 16, 0, 0, 40, 3, 328, 60, 0, 0, 0, 0];

    #[derive(Debug)]
    struct Config;

    impl TerminalConfiguration for Config {
        fn color_palette(&self) -> wezterm_term::color::ColorPalette {
            Default::default()
        }
    }

    fn terminal() -> Terminal {
        Terminal::new(
            TerminalSize {
                rows: 3,
                cols: 40,
                dpi: 96,
                pixel_width: 320,
                pixel_height: 48,
            },
            Arc::new(Config),
            "metal-hyperlinks",
            "test",
            Box::new(Vec::<u8>::new()),
        )
    }

    struct Owner(Arc<dyn Pane>);

    impl Owner {
        fn new(id: mux::pane::PaneId) -> Self {
            let pair = portable_pty::native_pty_system()
                .openpty(portable_pty::PtySize::default())
                .unwrap();
            let writer = pair.master.take_writer().unwrap();
            let child = pair
                .slave
                .spawn_command(portable_pty::CommandBuilder::new("/bin/cat"))
                .unwrap();
            Self(Arc::new(mux::localpane::LocalPane::new(
                id,
                terminal(),
                child,
                pair.master,
                writer,
                id,
                [0x52; 16],
                "metal hyperlinks".into(),
            )))
        }
    }

    impl Drop for Owner {
        fn drop(&mut self) {
            self.0.kill();
        }
    }

    fn capture(term: &mut Terminal, mirror: &mut RenderMirror, generation: usize, detect: bool) {
        let rules = if detect {
            vec![termwiz::hyperlink::Rule::new(r"https://[a-z.]+/", "$0").unwrap()]
        } else {
            Vec::new()
        };
        capture_terminal_rows(
            term,
            mirror,
            &CaptureRequest {
                viewport_top: None,
                rules: &rules,
                rules_generation: generation,
            },
        )
        .unwrap();
    }

    fn background() -> BackgroundUniforms {
        BackgroundUniforms {
            cell_size: [8.0, 16.0],
            grid_origin: [4.0, 6.0],
            ..BackgroundUniforms::default()
        }
    }

    fn update(
        cache: &mut HyperlinkRowCache,
        owner: &Owner,
        mirror: &RenderMirror,
        composition: Option<&str>,
    ) -> Arc<PaneHyperlinks> {
        cache
            .update(
                &owner.0,
                mirror,
                Some(GEOMETRY),
                PixelRect::new(0, 0, 328, 60),
                &background(),
                composition,
            )
            .unwrap()
    }

    fn event(frame: u64, shown: bool) -> FramePresentation {
        let now = Instant::now();
        FramePresentation {
            frame,
            size: [328, 60],
            submitted_at: now,
            presented_time: if shown { 1.0 } else { 0.0 },
            presented_at: shown.then_some(now),
        }
    }

    #[test]
    fn callbacks_reject_skips_stale_frames_and_retired_renderer_epochs() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<PaneHyperlinks>();
        send_sync::<MetalHyperlinks>();

        let state = Arc::new(MetalHyperlinks::default());
        let epoch = state.begin_epoch();
        let observed = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&observed);
        let old = state.handler(
            epoch,
            Arc::from([]),
            Some(Arc::new(move |_| {
                count.fetch_add(1, Ordering::Relaxed);
            })),
        );
        old(event(1, false));
        assert!(state.state.lock().unwrap().latest.is_none());
        old(event(3, true));
        old(event(2, true));
        old(event(4, false));
        assert_eq!(
            state.state.lock().unwrap().latest.as_ref().unwrap().frame,
            3
        );
        assert_eq!(
            observed.load(Ordering::Relaxed),
            4,
            "lifecycle observer runs exactly once"
        );

        let next = state.begin_epoch();
        old(event(100, true));
        assert!(state.state.lock().unwrap().latest.is_none());
        let current = state.handler(next, Arc::from([]), None);
        current(event(0, true));
        assert_eq!(
            state.state.lock().unwrap().latest.as_ref().unwrap().frame,
            0
        );
        state.retire_epoch(epoch);
        assert!(
            state.state.lock().unwrap().latest.is_some(),
            "old owner cannot retire its replacement"
        );
        state.retire_epoch(next);
        current(event(1, true));
        assert!(state.state.lock().unwrap().latest.is_none());

        let weak = Arc::downgrade(&state);
        drop(state);
        assert!(
            weak.upgrade().is_none(),
            "callbacks must not retain the rendering owner"
        );
        old(event(101, true));
        assert_eq!(observed.load(Ordering::Relaxed), 6);
    }

    #[test]
    fn dirty_rows_share_unchanged_spans_and_only_presented_snapshots_supply_links() {
        let owner = Owner::new(998_720);
        let mut term = terminal();
        term.advance_bytes(b"\x1b]8;;https://old.example/\x07old\x1b]8;;\x07\r\nhttps://implicit.example/\r\nplain");
        let mut mirror = RenderMirror::new();
        capture(&mut term, &mut mirror, 0, true);
        let mut cache = HyperlinkRowCache::default();
        let first = update(&mut cache, &owner, &mirror, None);
        assert_eq!(cache.rebuilt, 3);
        let second_row = Arc::clone(&cache.rows[&(mirror.first() + 1)].spans);
        let idle = update(&mut cache, &owner, &mirror, None);
        assert_eq!(cache.rebuilt, 0);
        assert!(Arc::ptr_eq(&first, &idle));
        assert_eq!(
            first.link_at(&owner.0, GEOMETRY, [5.0, 7.0]).unwrap().uri(),
            "https://old.example/"
        );
        assert_eq!(
            first
                .link_at(&owner.0, GEOMETRY, [5.0, 23.0])
                .unwrap()
                .uri(),
            "https://implicit.example/"
        );

        let displayed = Arc::new(MetalHyperlinks::default());
        let epoch = displayed.begin_epoch();
        let old_frame = displayed.handler(epoch, Arc::from([Arc::clone(&first)]), None);
        assert!(displayed.link_at(&owner.0, GEOMETRY, [5.0, 7.0]).is_none());
        old_frame(event(1, true));

        term.advance_bytes(b"\x1b[1;1H\x1b]8;;https://new.example/\x07new\x1b]8;;\x07");
        capture(&mut term, &mut mirror, 0, true);
        let next = update(&mut cache, &owner, &mirror, None);
        assert_eq!(cache.rebuilt, 1);
        assert!(Arc::ptr_eq(
            &second_row,
            &cache.rows[&(mirror.first() + 1)].spans
        ));
        assert_eq!(
            first.link_at(&owner.0, GEOMETRY, [5.0, 7.0]).unwrap().uri(),
            "https://old.example/"
        );
        let new_frame = displayed.handler(epoch, Arc::from([Arc::clone(&next)]), None);
        new_frame(event(2, false));
        assert_eq!(
            displayed
                .link_at(&owner.0, GEOMETRY, [5.0, 7.0])
                .unwrap()
                .uri(),
            "https://old.example/"
        );
        new_frame(event(2, true));
        old_frame(event(1, true));
        assert_eq!(
            displayed
                .link_at(&owner.0, GEOMETRY, [5.0, 7.0])
                .unwrap()
                .uri(),
            "https://new.example/"
        );

        let mut resized = GEOMETRY;
        resized[4] += 1;
        assert!(displayed.link_at(&owner.0, resized, [5.0, 7.0]).is_none());
        let mismatch = FramePresentation {
            size: [327, 60],
            ..event(3, true)
        };
        new_frame(mismatch);
        assert!(displayed.link_at(&owner.0, GEOMETRY, [5.0, 7.0]).is_none());

        // A rules edit changes no parser sequence, but the mirror refreshes
        // its resolved row generations and the clickable targets must follow.
        let sequence = mirror.seqno();
        capture(&mut term, &mut mirror, 1, false);
        assert_eq!(mirror.seqno(), sequence);
        let rules_changed = update(&mut cache, &owner, &mirror, None);
        assert_eq!(cache.rebuilt, 3);
        assert!(
            rules_changed
                .link_at(&owner.0, GEOMETRY, [5.0, 23.0])
                .is_none()
        );
        assert!(first.link_at(&owner.0, GEOMETRY, [5.0, 23.0]).is_some());

        let replacement = Owner::new(owner.0.pane_id());
        assert!(
            first
                .link_at(&replacement.0, GEOMETRY, [5.0, 7.0])
                .is_none()
        );
        let mux = Arc::new(mux::Mux::new(None));
        mux.add_pane(&owner.0).unwrap();
        assert!(first.link_at(&owner.0, GEOMETRY, [5.0, 7.0]).is_none());
        let registered = update(&mut cache, &owner, &mirror, None);
        assert!(registered.link_at(&owner.0, GEOMETRY, [5.0, 7.0]).is_some());
        assert!(
            mux.capture_pane_registration(&owner.0)
                .unwrap()
                .retire_if_current()
        );
        assert!(registered.link_at(&owner.0, GEOMETRY, [5.0, 7.0]).is_none());

        displayed.handler(epoch, Arc::from([]), None)(event(4, true));
        assert!(
            displayed
                .state
                .lock()
                .unwrap()
                .latest
                .as_ref()
                .unwrap()
                .panes
                .is_empty()
        );
    }

    #[test]
    fn dec_bottom_half_and_preedit_use_the_drawn_grid_and_clip() {
        let owner = Owner::new(998_721);
        let mut term = terminal();
        term.advance_bytes("\x1b#4\x1b]8;;https://wide.example/\x07界a\x1b]8;;\x07\x1b[1;2H");
        let mut mirror = RenderMirror::new();
        capture(&mut term, &mut mirror, 0, false);
        assert_eq!(
            mirror.rows()[0].line_mode(),
            MirrorLineMode::DoubleHeightBottom
        );
        let mut cache = HyperlinkRowCache::default();
        let clip = PixelRect::new(4, 6, 36, 48);
        let mut draw = |composition| {
            cache
                .update(
                    &owner.0,
                    &mirror,
                    Some(GEOMETRY),
                    clip,
                    &background(),
                    composition,
                )
                .unwrap()
        };
        let full = draw(None);
        assert!(full.link_at(&owner.0, GEOMETRY, [4.0, 7.0]).is_some());
        assert!(full.link_at(&owner.0, GEOMETRY, [35.0, 7.0]).is_some());
        assert!(full.link_at(&owner.0, GEOMETRY, [39.0, 7.0]).is_some());
        assert!(full.link_at(&owner.0, GEOMETRY, [40.0, 7.0]).is_none());
        assert!(full.link_at(&owner.0, GEOMETRY, [3.0, 7.0]).is_none());
        assert!(full.link_at(&owner.0, GEOMETRY, [5.0, 5.0]).is_none());
        let preedit = draw(Some("x"));
        assert!(preedit.link_at(&owner.0, GEOMETRY, [19.0, 7.0]).is_some());
        assert!(preedit.link_at(&owner.0, GEOMETRY, [20.0, 7.0]).is_none());
        assert!(preedit.link_at(&owner.0, GEOMETRY, [35.0, 7.0]).is_none());
        assert!(preedit.link_at(&owner.0, GEOMETRY, [36.0, 7.0]).is_some());
        assert!(
            Arc::ptr_eq(&preedit, &draw(Some("y"))),
            "preedit text with the same covered cells reuses hit spans"
        );
        for zero_width in ["\u{2028}", "\u{301}"] {
            assert_eq!(termwiz::cell::unicode_column_width(zero_width, None), 0);
            let overwritten = draw(Some(zero_width));
            assert!(Arc::ptr_eq(&preedit, &overwritten));
            assert!(
                overwritten
                    .link_at(&owner.0, GEOMETRY, [20.0, 7.0])
                    .is_none()
            );
            assert!(
                overwritten
                    .link_at(&owner.0, GEOMETRY, [36.0, 7.0])
                    .is_some()
            );
        }
        assert!(
            draw(None)
                .link_at(&owner.0, GEOMETRY, [20.0, 7.0])
                .is_some()
        );
        assert_eq!(cache.rebuilt, 1);
    }
}
