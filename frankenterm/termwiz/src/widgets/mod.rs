// Ideally this would be scoped to WidgetId, but I can't seem to find the
// right place for it to take effect
#![allow(clippy::new_without_default)]
use crate::Result;
use crate::color::ColorAttribute;
use crate::input::InputEvent;
use crate::surface::{
    Change, CursorShape, CursorVisibility, DirtyRect, Position, SequenceNo, Surface,
};
use fnv::FnvHasher;
use std::collections::{HashMap, VecDeque};
use std::hash::BuildHasherDefault;
use std::sync::{OnceLock, RwLock};

/// fnv is a more appropriate hasher for the WidgetIds we use in this module.
type FnvHashMap<K, V> = HashMap<K, V, BuildHasherDefault<FnvHasher>>;

pub mod layout;

/// Describes an event that may need to be processed by the widget
pub enum WidgetEvent {
    Input(InputEvent),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CursorShapeAndPosition {
    pub shape: CursorShape,
    pub coords: ParentRelativeCoords,
    pub color: ColorAttribute,
    pub visibility: CursorVisibility,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

pub struct RenderArgs<'a> {
    /// The id of the current widget
    pub id: WidgetId,
    pub is_focused: bool,
    pub cursor: &'a mut CursorShapeAndPosition,
    pub surface: &'a mut Surface,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RenderTelemetry {
    /// Number of widgets rendered during the most recent render pass.
    pub widgets_rendered: usize,
    /// Number of dirty rectangles emitted while compositing widget surfaces.
    pub widget_dirty_rects: usize,
    /// Total dirty cells emitted while compositing widget surfaces.
    pub widget_dirty_cells: usize,
    /// Number of tile-aligned dirty regions emitted while compositing widget surfaces.
    pub widget_dirty_tiles: usize,
    /// Total dirty cells emitted in tile-aligned widget regions.
    pub widget_dirty_tile_cells: usize,
    /// Number of dirty widget regions selected for upload in the active render mode.
    pub widget_upload_regions: usize,
    /// Total dirty widget cells selected for upload in the active render mode.
    pub widget_upload_cells: usize,
    /// Number of dirty rectangles between the previous and current composed frame.
    pub frame_dirty_rects: usize,
    /// Total dirty cells between the previous and current composed frame.
    pub frame_dirty_cells: usize,
    /// Number of tile-aligned dirty regions between the previous and current composed frame.
    pub frame_dirty_tiles: usize,
    /// Total dirty cells across tile-aligned frame regions.
    pub frame_dirty_tile_cells: usize,
    /// Number of dirty regions selected for upload in the active render mode.
    pub frame_upload_regions: usize,
    /// Total dirty cells selected for upload in the active render mode.
    pub frame_upload_cells: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderUploadMode {
    Rects,
    Tiles {
        tile_width: usize,
        tile_height: usize,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderUploadSnapshot {
    /// Active dirty-region mode used by the last render pass.
    pub mode: RenderUploadMode,
    /// Dirty upload regions selected from composited widget surfaces.
    pub widget_regions: usize,
    /// Total dirty widget cells selected for upload.
    pub widget_cells: usize,
    /// Dirty upload regions selected from composed frame diff.
    pub frame_regions: usize,
    /// Total dirty frame cells selected for upload.
    pub frame_cells: usize,
}

impl RenderUploadSnapshot {
    fn from_telemetry(telemetry: RenderTelemetry, mode: DirtyRegionMode) -> Self {
        Self {
            mode: mode.into(),
            widget_regions: telemetry.widget_upload_regions,
            widget_cells: telemetry.widget_upload_cells,
            frame_regions: telemetry.frame_upload_regions,
            frame_cells: telemetry.frame_upload_cells,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DirtyRegionMode {
    Rects,
    Tiles {
        tile_width: usize,
        tile_height: usize,
    },
}

impl From<DirtyRegionMode> for RenderUploadMode {
    fn from(mode: DirtyRegionMode) -> Self {
        match mode {
            DirtyRegionMode::Rects => Self::Rects,
            DirtyRegionMode::Tiles {
                tile_width,
                tile_height,
            } => Self::Tiles {
                tile_width,
                tile_height,
            },
        }
    }
}

static GLOBAL_RENDER_UPLOAD_SNAPSHOT: OnceLock<RwLock<Option<RenderUploadSnapshot>>> =
    OnceLock::new();

/// UpdateArgs provides access to the widget and UI state during
/// a call to `Widget::update_state`
pub struct UpdateArgs<'a> {
    /// The id of the current widget
    pub id: WidgetId,
    pub cursor: &'a mut CursorShapeAndPosition,
}

/// Implementing the `Widget` trait allows for defining a potentially
/// interactive component in a UI layout.
pub trait Widget {
    /// Draw the widget to the RenderArgs::surface, and optionally
    /// update RenderArgs::cursor to reflect the cursor position and
    /// display attributes.
    fn render(&mut self, args: &mut RenderArgs);

    /// Override this to have your widget specify its layout constraints.
    /// You may wish to have your widget constructor receive a `Constraints`
    /// instance to make this more easily configurable in more generic widgets.
    fn get_size_constraints(&self) -> layout::Constraints {
        Default::default()
    }

    /// Override this to allow your widget to respond to keyboard, mouse and
    /// other widget events.
    /// Return `true` if your widget handled the event, or `false` to allow
    /// the event to propagate to the widget parent.
    fn process_event(&mut self, _event: &WidgetEvent, _args: &mut UpdateArgs) -> bool {
        false
    }
}

/// Relative to the top left of the parent container
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ParentRelativeCoords {
    pub x: usize,
    pub y: usize,
}

impl ParentRelativeCoords {
    pub fn new(x: usize, y: usize) -> Self {
        Self { x, y }
    }
}

impl From<(usize, usize)> for ParentRelativeCoords {
    fn from(coords: (usize, usize)) -> ParentRelativeCoords {
        ParentRelativeCoords::new(coords.0, coords.1)
    }
}

/// Relative to the top left of the screen
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScreenRelativeCoords {
    pub x: usize,
    pub y: usize,
}

impl ScreenRelativeCoords {
    pub fn new(x: usize, y: usize) -> Self {
        Self { x, y }
    }

    pub fn offset_by(&self, rel: &ParentRelativeCoords) -> Self {
        Self {
            x: self.x + rel.x,
            y: self.y + rel.y,
        }
    }
}

static WIDGET_ID: ::std::sync::atomic::AtomicUsize = ::std::sync::atomic::AtomicUsize::new(0);

/// The `WidgetId` uniquely describes an instance of a widget.
/// Creating a new `WidgetId` generates a new unique identifier which can
/// be safely copied and moved around; each copy refers to the same widget.
/// The intent is that you set up the identifiers once and re-use them,
/// rather than generating new ids on each iteration of the UI loop so that
/// the widget state is maintained correctly by the Ui.
#[derive(Copy, Clone, Hash, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct WidgetId(usize);

impl WidgetId {
    pub fn new() -> Self {
        WidgetId(WIDGET_ID.fetch_add(1, ::std::sync::atomic::Ordering::Relaxed))
    }
}

impl Default for WidgetId {
    fn default() -> Self {
        Self::new()
    }
}

struct RenderData<'widget> {
    surface: Surface,
    cursor: CursorShapeAndPosition,
    coordinates: ParentRelativeCoords,
    widget: Box<dyn Widget + 'widget>,
}

#[derive(Default)]
struct Graph {
    root: Option<WidgetId>,
    children: FnvHashMap<WidgetId, Vec<WidgetId>>,
    parent: FnvHashMap<WidgetId, WidgetId>,
}

impl Graph {
    fn add(&mut self, parent: Option<WidgetId>) -> WidgetId {
        let id = WidgetId::new();

        if self.root.is_none() {
            self.root = Some(id);
        }

        self.children.insert(id, Vec::new());

        if let Some(parent) = parent {
            self.parent.insert(id, parent);
            self.children.get_mut(&parent).unwrap().push(id);
        }

        id
    }

    fn children(&self, id: WidgetId) -> &[WidgetId] {
        self.children
            .get(&id)
            .map(|v| v.as_slice())
            .unwrap_or_else(|| &[])
    }
}

/// Manages the widgets on the display
#[derive(Default)]
pub struct Ui<'widget> {
    graph: Graph,
    render: FnvHashMap<WidgetId, RenderData<'widget>>,
    input_queue: VecDeque<WidgetEvent>,
    focused: Option<WidgetId>,
    last_render_telemetry: RenderTelemetry,
    last_render_upload_snapshot: Option<RenderUploadSnapshot>,
}

impl<'widget> Ui<'widget> {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn add<W: Widget + 'widget>(&mut self, parent: Option<WidgetId>, w: W) -> WidgetId {
        let id = self.graph.add(parent);

        self.render.insert(
            id,
            RenderData {
                surface: Surface::new(1, 1),
                cursor: Default::default(),
                coordinates: Default::default(),
                widget: Box::new(w),
            },
        );

        if parent.is_none() && self.focused.is_none() {
            self.focused = Some(id);
        }

        id
    }

    pub fn set_root<W: Widget + 'widget>(&mut self, w: W) -> WidgetId {
        self.add(None, w)
    }

    pub fn add_child<W: Widget + 'widget>(&mut self, parent: WidgetId, w: W) -> WidgetId {
        self.add(Some(parent), w)
    }

    fn do_deliver(&mut self, id: WidgetId, event: &WidgetEvent) -> bool {
        let render_data = self.render.get_mut(&id).unwrap();
        let mut args = UpdateArgs {
            id,
            cursor: &mut render_data.cursor,
        };

        render_data.widget.process_event(event, &mut args)
    }

    fn deliver_event(&mut self, mut id: WidgetId, event: &WidgetEvent) {
        loop {
            let handled = match event {
                WidgetEvent::Input(InputEvent::Resized { .. }) => true,
                WidgetEvent::Input(InputEvent::Mouse(m)) => {
                    let mut m = m.clone();
                    // convert from screen to widget coords
                    let coords = self.to_widget_coords(
                        id,
                        &ScreenRelativeCoords::new(m.x as usize, m.y as usize),
                    );
                    m.x = coords.x as u16;
                    m.y = coords.y as u16;
                    self.do_deliver(id, &WidgetEvent::Input(InputEvent::Mouse(m)))
                }
                WidgetEvent::Input(InputEvent::Paste(_))
                | WidgetEvent::Input(InputEvent::PixelMouse(_))
                | WidgetEvent::Input(InputEvent::Key(_))
                | WidgetEvent::Input(InputEvent::Wake) => self.do_deliver(id, event),
            };

            if handled {
                return;
            }

            id = match self.graph.parent.get(&id) {
                Some(parent) => *parent,
                None => return,
            };
        }
    }

    /// find the best matching widget that is under the mouse cursor.
    /// We're looking for the latest, deepest widget that contains the input
    /// coordinates.
    fn hovered_widget(&self, coords: &ScreenRelativeCoords) -> Option<WidgetId> {
        let root = match self.graph.root {
            Some(id) => id,
            _ => return None,
        };

        let depth = 0;
        let mut best = (depth, root);
        self.hovered_recursive(root, depth, coords.x, coords.y, &mut best);

        Some(best.1)
    }

    /// Recursive helper for hovered_widget().  The `best` tuple holds the
    /// best (depth, widget) pair.  Depth is incremented each time the function
    /// recurses.
    fn hovered_recursive(
        &self,
        widget: WidgetId,
        depth: usize,
        x: usize,
        y: usize,
        best: &mut (usize, WidgetId),
    ) {
        let render = &self.render[&widget];

        // only consider the dimensions if this node is at the same or a deeper
        // depth.  If so, then we check to see if the coords are within the bounds.
        if depth >= best.0 && x >= render.coordinates.x && y >= render.coordinates.y {
            let (width, height) = render.surface.dimensions();

            if (x - render.coordinates.x < width) && (y - render.coordinates.y < height) {
                *best = (depth, widget);
            }
        }

        for child in self.graph.children(widget) {
            self.hovered_recursive(
                *child,
                depth + 1,
                x + render.coordinates.x,
                y + render.coordinates.y,
                best,
            );
        }
    }

    pub fn process_event_queue(&mut self) -> Result<()> {
        while let Some(event) = self.input_queue.pop_front() {
            match event {
                WidgetEvent::Input(InputEvent::Resized { rows, cols }) => {
                    self.compute_layout(cols, rows)?;
                }
                WidgetEvent::Input(InputEvent::Mouse(ref m)) => {
                    if let Some(hover) =
                        self.hovered_widget(&ScreenRelativeCoords::new(m.x as usize, m.y as usize))
                    {
                        self.deliver_event(hover, &event);
                    }
                }
                WidgetEvent::Input(InputEvent::Key(_))
                | WidgetEvent::Input(InputEvent::Paste(_))
                | WidgetEvent::Input(InputEvent::PixelMouse(_))
                | WidgetEvent::Input(InputEvent::Wake) => {
                    if let Some(focus) = self.focused {
                        self.deliver_event(focus, &event);
                    }
                }
            }
        }
        Ok(())
    }

    /// Queue up an event.  Events are processed by the appropriate
    /// `Widget::update_state` method.  Events may be re-processed to
    /// simplify handling for widgets. eg: a TODO: is to synthesize double
    /// and triple click events.
    pub fn queue_event(&mut self, event: WidgetEvent) {
        self.input_queue.push_back(event);
    }

    /// Assign keyboard focus to the specified widget.
    pub fn set_focus(&mut self, id: WidgetId) {
        self.focused = Some(id);
    }

    fn accumulate_dirty_rects(rects: &[DirtyRect]) -> (usize, usize) {
        let count = rects.len();
        let cells = rects.iter().fold(0usize, |acc, rect| {
            acc.saturating_add(rect.width.saturating_mul(rect.height))
        });
        (count, cells)
    }

    /// Helper for applying the surfaces from the widgets to the target
    /// screen in the correct order (from the root to the leaves)
    fn render_recursive(
        &mut self,
        id: WidgetId,
        screen: &mut Surface,
        abs_coords: &ScreenRelativeCoords,
        mode: DirtyRegionMode,
        telemetry: &mut RenderTelemetry,
    ) -> Result<()> {
        let coords = {
            let render_data = self.render.get_mut(&id).unwrap();
            let surface = &mut render_data.surface;
            {
                let mut args = RenderArgs {
                    id,
                    cursor: &mut render_data.cursor,
                    surface,
                    is_focused: self.focused.map(|f| f == id).unwrap_or(false),
                };
                render_data.widget.render(&mut args);
            }
            let dirty_regions = match mode {
                DirtyRegionMode::Rects => {
                    screen
                        .draw_from_screen_with_dirty_rects(
                            surface,
                            abs_coords.x + render_data.coordinates.x,
                            abs_coords.y + render_data.coordinates.y,
                        )
                        .1
                }
                DirtyRegionMode::Tiles {
                    tile_width,
                    tile_height,
                } => {
                    screen
                        .draw_from_screen_with_dirty_tiles(
                            surface,
                            abs_coords.x + render_data.coordinates.x,
                            abs_coords.y + render_data.coordinates.y,
                            tile_width,
                            tile_height,
                        )
                        .1
                }
            };
            let (region_count, dirty_cells) = Self::accumulate_dirty_rects(&dirty_regions);
            telemetry.widgets_rendered = telemetry.widgets_rendered.saturating_add(1);
            match mode {
                DirtyRegionMode::Rects => {
                    telemetry.widget_dirty_rects =
                        telemetry.widget_dirty_rects.saturating_add(region_count);
                    telemetry.widget_dirty_cells =
                        telemetry.widget_dirty_cells.saturating_add(dirty_cells);
                }
                DirtyRegionMode::Tiles { .. } => {
                    telemetry.widget_dirty_tiles =
                        telemetry.widget_dirty_tiles.saturating_add(region_count);
                    telemetry.widget_dirty_tile_cells = telemetry
                        .widget_dirty_tile_cells
                        .saturating_add(dirty_cells);
                }
            }
            telemetry.widget_upload_regions =
                telemetry.widget_upload_regions.saturating_add(region_count);
            telemetry.widget_upload_cells =
                telemetry.widget_upload_cells.saturating_add(dirty_cells);
            surface.flush_changes_older_than(SequenceNo::MAX);
            render_data.coordinates
        };

        for child in self.graph.children(id).to_vec() {
            self.render_recursive(
                child,
                screen,
                &ScreenRelativeCoords::new(coords.x + abs_coords.x, coords.y + abs_coords.y),
                mode,
                telemetry,
            )?;
        }

        Ok(())
    }

    /// Reconsider the layout constraints and apply them.
    /// Returns true if the layout was changed, false if no changes were made.
    fn compute_layout(&mut self, width: usize, height: usize) -> Result<bool> {
        let mut layout = layout::LayoutState::new();

        let root = self.graph.root.unwrap();
        self.add_widget_to_layout(&mut layout, root)?;
        let mut changed = false;

        // Clippy is dead wrong about this iterator being an identity_conversion
        #[allow(clippy::useless_conversion)]
        for result in layout.compute_constraints(width, height, root)? {
            let render_data = self.render.get_mut(&result.widget).unwrap();
            let coords = ParentRelativeCoords::new(result.rect.x, result.rect.y);
            if coords != render_data.coordinates {
                render_data.coordinates = coords;
                changed = true;
            }

            if (result.rect.width, result.rect.height) != render_data.surface.dimensions() {
                render_data
                    .surface
                    .resize(result.rect.width, result.rect.height);
                changed = true;
            }
        }
        Ok(changed)
    }

    /// Recursive helper for building up the LayoutState
    fn add_widget_to_layout(
        &mut self,
        layout: &mut layout::LayoutState,
        widget: WidgetId,
    ) -> Result<()> {
        let constraints = self.render[&widget].widget.get_size_constraints();
        let children = self.graph.children(widget).to_vec();

        layout.add_widget(widget, &constraints, &children);

        for child in children {
            self.add_widget_to_layout(layout, child)?;
        }
        Ok(())
    }

    fn render_to_screen_with_dirty_region_mode(
        &mut self,
        screen: &mut Surface,
        mode: DirtyRegionMode,
    ) -> Result<(bool, Vec<DirtyRect>)> {
        let mut frame_dirty_regions = Vec::new();
        if let Some(root) = self.graph.root {
            let mut telemetry = RenderTelemetry::default();
            let (width, height) = screen.dimensions();
            // Render from scratch into a fresh screen buffer
            let mut alt_screen = Surface::new(width, height);
            self.render_recursive(
                root,
                &mut alt_screen,
                &ScreenRelativeCoords::new(0, 0),
                mode,
                &mut telemetry,
            )?;
            let frame_dirty_rects = screen.dirty_rects(&alt_screen);
            let (frame_rect_count, frame_dirty_cells) =
                Self::accumulate_dirty_rects(&frame_dirty_rects);
            telemetry.frame_dirty_rects = frame_rect_count;
            telemetry.frame_dirty_cells = frame_dirty_cells;
            frame_dirty_regions = match mode {
                DirtyRegionMode::Rects => frame_dirty_rects,
                DirtyRegionMode::Tiles {
                    tile_width,
                    tile_height,
                } => {
                    let frame_dirty_tiles =
                        screen.dirty_tiles(&alt_screen, tile_width, tile_height);
                    let (frame_tile_count, frame_dirty_tile_cells) =
                        Self::accumulate_dirty_rects(&frame_dirty_tiles);
                    telemetry.frame_dirty_tiles = frame_tile_count;
                    telemetry.frame_dirty_tile_cells = frame_dirty_tile_cells;
                    frame_dirty_tiles
                }
            };
            let (upload_region_count, upload_dirty_cells) =
                Self::accumulate_dirty_rects(&frame_dirty_regions);
            telemetry.frame_upload_regions = upload_region_count;
            telemetry.frame_upload_cells = upload_dirty_cells;
            // Now compute a delta and apply it to the actual screen
            let diff = screen.diff_screens(&alt_screen);
            screen.add_changes(diff);
            let upload_snapshot = RenderUploadSnapshot::from_telemetry(telemetry, mode);
            self.last_render_telemetry = telemetry;
            self.last_render_upload_snapshot = Some(upload_snapshot);
            update_global_render_upload_snapshot(upload_snapshot);
        }
        // TODO: garbage collect unreachable WidgetId's from self.state

        if let Some(id) = self.focused {
            let cursor = &self.render[&id].cursor;
            let coords = self.to_screen_coords(id, &cursor.coords);

            screen.add_changes(vec![
                Change::CursorShape(cursor.shape),
                Change::CursorColor(cursor.color),
                Change::CursorVisibility(cursor.visibility),
                Change::CursorPosition {
                    x: Position::Absolute(coords.x),
                    y: Position::Absolute(coords.y),
                },
            ]);
        }

        let (width, height) = screen.dimensions();
        let needs_update = self.compute_layout(width, height)?;
        Ok((needs_update, frame_dirty_regions))
    }

    /// Apply the current state of the widgets to the screen.
    /// This has the side effect of clearing out any unconsumed input queue.
    /// Returns a tuple of:
    /// - whether the Ui may need another update pass (e.g. layout changed)
    /// - dirty rectangles for the composed frame diff, suitable for partial uploads
    pub fn render_to_screen_with_dirty_rects(
        &mut self,
        screen: &mut Surface,
    ) -> Result<(bool, Vec<DirtyRect>)> {
        self.render_to_screen_with_dirty_region_mode(screen, DirtyRegionMode::Rects)
    }

    /// Like [`Ui::render_to_screen_with_dirty_rects`], but returns tile-aligned
    /// dirty regions for the composed frame diff.
    pub fn render_to_screen_with_dirty_tiles(
        &mut self,
        screen: &mut Surface,
        tile_width: usize,
        tile_height: usize,
    ) -> Result<(bool, Vec<DirtyRect>)> {
        self.render_to_screen_with_dirty_region_mode(
            screen,
            DirtyRegionMode::Tiles {
                tile_width,
                tile_height,
            },
        )
    }

    /// Apply the current state of the widgets to the screen.
    /// This has the side effect of clearing out any unconsumed input queue.
    /// Returns true if the Ui may need to be updated again; for example,
    /// if the most recent update operation changed layout.
    pub fn render_to_screen(&mut self, screen: &mut Surface) -> Result<bool> {
        let (needs_update, _) = self.render_to_screen_with_dirty_rects(screen)?;
        Ok(needs_update)
    }

    /// Telemetry from the last render pass.
    pub fn last_render_telemetry(&self) -> RenderTelemetry {
        self.last_render_telemetry
    }

    /// Dirty upload mode + region/cell totals from the most recent render pass.
    pub fn last_render_upload_snapshot(&self) -> Option<RenderUploadSnapshot> {
        self.last_render_upload_snapshot
    }

    fn coord_walk<F: Fn(usize, usize) -> usize>(
        &self,
        widget: WidgetId,
        mut x: usize,
        mut y: usize,
        f: F,
    ) -> (usize, usize) {
        let mut widget = widget;
        loop {
            let render = &self.render[&widget];
            x = f(x, render.coordinates.x);
            y = f(y, render.coordinates.y);

            widget = match self.graph.parent.get(&widget) {
                Some(parent) => *parent,
                None => break,
            };
        }
        (x, y)
    }

    /// Convert coordinates that are relative to widget into coordinates
    /// that are relative to the screen origin (top left).
    pub fn to_screen_coords(
        &self,
        widget: WidgetId,
        coords: &ParentRelativeCoords,
    ) -> ScreenRelativeCoords {
        let (x, y) = self.coord_walk(widget, coords.x, coords.y, |a, b| a + b);
        ScreenRelativeCoords { x, y }
    }

    /// Convert coordinates that are relative to the screen origin (top left)
    /// into coordinates that are relative to the widget.
    pub fn to_widget_coords(
        &self,
        widget: WidgetId,
        coords: &ScreenRelativeCoords,
    ) -> ParentRelativeCoords {
        let (x, y) = self.coord_walk(widget, coords.x, coords.y, |a, b| a - b);
        ParentRelativeCoords { x, y }
    }
}
#[cfg(test)]
mod test {
    use super::*;

    struct CursorHider {}

    impl Widget for CursorHider {
        fn render(&mut self, args: &mut RenderArgs) {
            args.cursor.visibility = CursorVisibility::Hidden;
        }
    }

    #[test]
    fn hide_cursor() {
        let mut ui = Ui::new();

        ui.set_root(CursorHider {});

        let mut surface = Surface::new(10, 10);
        assert_eq!(CursorVisibility::Visible, surface.cursor_visibility());
        ui.render_to_screen(&mut surface).unwrap();
        assert_eq!(CursorVisibility::Hidden, surface.cursor_visibility());
    }

    struct PaintCell;

    impl Widget for PaintCell {
        fn render(&mut self, args: &mut RenderArgs) {
            args.surface.add_changes(vec![
                Change::CursorPosition {
                    x: Position::Absolute(0),
                    y: Position::Absolute(0),
                },
                Change::Text("X".to_string()),
            ]);
        }
    }

    struct ChurnPainter {
        frame: usize,
    }

    impl Widget for ChurnPainter {
        fn render(&mut self, args: &mut RenderArgs) {
            let frame = self.frame;
            let x = frame % 4;
            let y = (frame / 2) % 4;
            let glyph = ((frame % 26) as u8 + b'A') as char;

            args.surface.add_changes(vec![
                Change::CursorPosition {
                    x: Position::Absolute(x),
                    y: Position::Absolute(y),
                },
                Change::Text(glyph.to_string()),
            ]);

            args.cursor.coords = ParentRelativeCoords::new(x, y);
            args.cursor.shape = if frame.is_multiple_of(2) {
                CursorShape::SteadyBlock
            } else {
                CursorShape::SteadyUnderline
            };
            args.cursor.visibility = CursorVisibility::Visible;
            self.frame = self.frame.saturating_add(1);
        }
    }

    #[test]
    fn render_telemetry_tracks_dirty_rects() {
        let mut ui = Ui::new();
        ui.set_root(PaintCell);

        let mut surface = Surface::new(4, 4);

        ui.render_to_screen(&mut surface).unwrap();
        let first = ui.last_render_telemetry();
        assert_eq!(first.widgets_rendered, 1);
        assert!(first.widget_dirty_rects >= 1);
        assert!(first.widget_dirty_cells >= 1);
        assert_eq!(first.widget_dirty_tiles, 0);
        assert_eq!(first.widget_dirty_tile_cells, 0);
        assert_eq!(first.widget_upload_regions, first.widget_dirty_rects);
        assert_eq!(first.widget_upload_cells, first.widget_dirty_cells);
        assert!(first.frame_dirty_rects >= 1);
        assert!(first.frame_dirty_cells >= 1);
        assert_eq!(first.frame_dirty_tiles, 0);
        assert_eq!(first.frame_dirty_tile_cells, 0);
        assert_eq!(first.frame_upload_regions, first.frame_dirty_rects);
        assert_eq!(first.frame_upload_cells, first.frame_dirty_cells);

        ui.render_to_screen(&mut surface).unwrap();
        let second = ui.last_render_telemetry();
        assert_eq!(second.widgets_rendered, 1);
        assert!(second.widget_dirty_rects >= 1);
        assert!(second.widget_dirty_cells >= 1);
        assert_eq!(second.widget_dirty_tiles, 0);
        assert_eq!(second.widget_dirty_tile_cells, 0);
        assert_eq!(second.widget_upload_regions, second.widget_dirty_rects);
        assert_eq!(second.widget_upload_cells, second.widget_dirty_cells);
        assert_eq!(second.frame_dirty_rects, 0);
        assert_eq!(second.frame_dirty_cells, 0);
        assert_eq!(second.frame_dirty_tiles, 0);
        assert_eq!(second.frame_dirty_tile_cells, 0);
        assert_eq!(second.frame_upload_regions, 0);
        assert_eq!(second.frame_upload_cells, 0);
    }

    #[test]
    fn render_to_screen_with_dirty_rects_reports_frame_regions() {
        let mut ui = Ui::new();
        ui.set_root(PaintCell);

        let mut surface = Surface::new(4, 4);

        let (_, first_rects) = ui.render_to_screen_with_dirty_rects(&mut surface).unwrap();
        assert!(!first_rects.is_empty());
        let first = ui.last_render_telemetry();
        let first_upload = ui
            .last_render_upload_snapshot()
            .expect("render pass should capture upload snapshot");
        assert_eq!(first_upload.mode, RenderUploadMode::Rects);
        assert_eq!(first_upload.widget_regions, first.widget_upload_regions);
        assert_eq!(first_upload.widget_cells, first.widget_upload_cells);
        assert_eq!(first_upload.frame_regions, first.frame_upload_regions);
        assert_eq!(first_upload.frame_cells, first.frame_upload_cells);
        assert_eq!(first.widget_upload_regions, first.widget_dirty_rects);
        assert_eq!(first.widget_upload_cells, first.widget_dirty_cells);
        assert_eq!(first.frame_upload_regions, first_rects.len());
        assert_eq!(first.frame_upload_regions, first.frame_dirty_rects);
        assert_eq!(first.frame_upload_cells, first.frame_dirty_cells);

        let (_, second_rects) = ui.render_to_screen_with_dirty_rects(&mut surface).unwrap();
        assert!(second_rects.is_empty());
        let second = ui.last_render_telemetry();
        assert_eq!(second.frame_upload_regions, 0);
        assert_eq!(second.frame_upload_cells, 0);
    }

    #[test]
    fn render_to_screen_with_dirty_tiles_reports_frame_regions() {
        let mut ui = Ui::new();
        ui.set_root(PaintCell);

        let mut surface = Surface::new(4, 4);

        let (_, first_tiles) = ui
            .render_to_screen_with_dirty_tiles(&mut surface, 2, 2)
            .unwrap();
        assert!(!first_tiles.is_empty());
        let first = ui.last_render_telemetry();
        let first_upload = ui
            .last_render_upload_snapshot()
            .expect("render pass should capture upload snapshot");
        assert_eq!(
            first_upload.mode,
            RenderUploadMode::Tiles {
                tile_width: 2,
                tile_height: 2
            }
        );
        assert_eq!(first_upload.widget_regions, first.widget_upload_regions);
        assert_eq!(first_upload.widget_cells, first.widget_upload_cells);
        assert_eq!(first_upload.frame_regions, first.frame_upload_regions);
        assert_eq!(first_upload.frame_cells, first.frame_upload_cells);
        assert!(first.widget_dirty_tiles >= 1);
        assert!(first.widget_dirty_tile_cells >= 1);
        assert_eq!(first.widget_dirty_rects, 0);
        assert_eq!(first.widget_dirty_cells, 0);
        assert_eq!(first.widget_upload_regions, first.widget_dirty_tiles);
        assert_eq!(first.widget_upload_cells, first.widget_dirty_tile_cells);
        assert!(first.frame_dirty_tiles >= 1);
        assert!(first.frame_dirty_tile_cells >= 1);
        assert_eq!(first.frame_upload_regions, first_tiles.len());
        assert_eq!(first.frame_upload_regions, first.frame_dirty_tiles);
        assert_eq!(first.frame_upload_cells, first.frame_dirty_tile_cells);

        let (_, second_tiles) = ui
            .render_to_screen_with_dirty_tiles(&mut surface, 2, 2)
            .unwrap();
        assert!(second_tiles.is_empty());
        let second = ui.last_render_telemetry();
        assert_eq!(second.widget_dirty_rects, 0);
        assert_eq!(second.widget_dirty_cells, 0);
        assert!(second.widget_dirty_tiles >= 1);
        assert!(second.widget_dirty_tile_cells >= 1);
        assert_eq!(second.widget_upload_regions, second.widget_dirty_tiles);
        assert_eq!(second.widget_upload_cells, second.widget_dirty_tile_cells);
        assert_eq!(second.frame_dirty_tiles, 0);
        assert_eq!(second.frame_dirty_tile_cells, 0);
        assert_eq!(second.frame_upload_regions, 0);
        assert_eq!(second.frame_upload_cells, 0);
    }

    #[test]
    fn render_modes_preserve_frame_semantics_under_churn() {
        let mut rect_ui = Ui::new();
        rect_ui.set_root(ChurnPainter { frame: 0 });
        let mut tile_ui = Ui::new();
        tile_ui.set_root(ChurnPainter { frame: 0 });

        let mut rect_surface = Surface::new(4, 4);
        let mut tile_surface = Surface::new(4, 4);

        for step in 0..8 {
            let (rect_needs_update, rect_regions) = rect_ui
                .render_to_screen_with_dirty_rects(&mut rect_surface)
                .unwrap();
            let (tile_needs_update, tile_regions) = tile_ui
                .render_to_screen_with_dirty_tiles(&mut tile_surface, 2, 2)
                .unwrap();

            assert_eq!(
                rect_needs_update, tile_needs_update,
                "layout update parity mismatch at step {step}"
            );
            assert_eq!(
                rect_regions.is_empty(),
                tile_regions.is_empty(),
                "frame-change parity mismatch at step {step}"
            );
            assert_eq!(
                rect_surface.screen_chars_to_string(),
                tile_surface.screen_chars_to_string(),
                "visible frame diverged at step {step}"
            );
            assert_eq!(
                rect_surface.cursor_position(),
                tile_surface.cursor_position(),
                "cursor position diverged at step {step}"
            );
            assert_eq!(
                rect_surface.cursor_shape(),
                tile_surface.cursor_shape(),
                "cursor shape diverged at step {step}"
            );
            assert_eq!(
                rect_surface.cursor_visibility(),
                tile_surface.cursor_visibility(),
                "cursor visibility diverged at step {step}"
            );
        }
    }

    #[test]
    fn global_render_upload_snapshot_tracks_latest_render_pass() {
        let mut ui = Ui::new();
        ui.set_root(PaintCell);
        let mut surface = Surface::new(4, 4);

        ui.render_to_screen_with_dirty_tiles(&mut surface, 2, 2)
            .unwrap();

        let global = global_render_upload_snapshot()
            .expect("global render/upload snapshot should be populated after render");
        assert_eq!(
            global.mode,
            RenderUploadMode::Tiles {
                tile_width: 2,
                tile_height: 2
            }
        );
        assert!(global.frame_regions >= 1);
        assert!(global.frame_cells >= 1);
    }
}
fn update_global_render_upload_snapshot(snapshot: RenderUploadSnapshot) {
    let lock = GLOBAL_RENDER_UPLOAD_SNAPSHOT.get_or_init(|| RwLock::new(None));
    if let Ok(mut guard) = lock.write() {
        *guard = Some(snapshot);
    }
}

/// Most recent render/upload snapshot recorded by any Ui in this process.
pub fn global_render_upload_snapshot() -> Option<RenderUploadSnapshot> {
    let lock = GLOBAL_RENDER_UPLOAD_SNAPSHOT.get_or_init(|| RwLock::new(None));
    lock.read().ok().and_then(|guard| *guard)
}
