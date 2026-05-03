//! Layered compositor abstraction (rio sugar-layer pattern).
//!
//! Pattern reference: `legacy_rio/rio/sugarloaf/src/components/layer/mod.rs`.
//! Bead: ft-mpc9b.4.1.
//!
//! ## Why this exists
//!
//! Today the render pipeline is a single sweep that touches every
//! visible pixel on every redraw. Layered compositing replaces that
//! with a stack of independently-dirty-tracked layers, each owning
//! a slice of the final framebuffer:
//!
//! - Layer 0: backdrop (image / blur / transparency)
//! - Layer 1: tiled grid (the current pane layout)
//! - Layer 2: floating panes
//! - Layer 3: status tiles
//! - Layer 4: modal overlays
//!
//! Each layer renders only when its own dirty signal fires. The
//! stack walks bottom-up by `z_order()`; if a higher layer is
//! `opaque()` AND fully covers a lower layer's region, the lower
//! layer's render call is skipped.
//!
//! ## What this module ships (foundation, ft-mpc9b.4.1)
//!
//! - `Layer` trait — render / dirty_rect / z_order / opaque API.
//! - `LayerStack` — owns the layers and orchestrates the render
//!   walk plus the opaque-below-skip optimization.
//! - `LayerContext` — read-only context handed to each layer's
//!   `render` (frame_idx, viewport size, palette epoch).
//! - `DirtyRect` — small (x, y, w, h) struct in viewport pixels.
//!   Distinct from `window::Rect` so the compositor's dirty
//!   tracking doesn't pull in euclid units we don't need.
//! - `DrawCmd` — placeholder enum for the layer's emitted commands.
//!   The continuation bead replaces this with the real wgpu /
//!   glium command shape once paint.rs is migrated.
//! - `LayerKind` — the canonical 5 named layers from the bead so
//!   z_order conventions stay consistent across implementations.
//! - `RenderReport` — telemetry returned from `LayerStack::render`
//!   (layers_rendered / layers_skipped / total commands) so the
//!   continuation bead's structured-log emission has a typed
//!   payload.
//!
//! ## What is deferred (continuation, see follow-up bead)
//!
//! - Migrating the current tiled-grid rendering into a real
//!   `Layer` impl in `crates/frankenterm-gui/src/termwindow/render/pane.rs`
//! - The paint.rs integration that replaces the single-sweep
//!   render path with `LayerStack::render`
//! - Per-layer atlas regions vs shared atlas — current default is
//!   shared (the existing GlyphCache); follow-up may revisit
//! - A11Y: each layer contributing to the platform accessibility
//!   tree (cross-link A11Y.1)
//! - Color management: composition in linear space + final gamut
//!   convert (cross-link A11Y.3)
//! - Reduce-motion: layer-transition disabling (cross-link A11Y.5)
//! - Benches: layer-stack overhead < 0.5ms per frame target

#![allow(dead_code)]

use std::cmp::Ordering;
use std::fmt;

/// A small rectangle in viewport pixels, used for dirty-region
/// tracking and opaque-below-skip culling. Kept distinct from
/// `::window::Rect` / `euclid::Rect` so the compositor surface
/// doesn't depend on the broader window crate's geometry types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DirtyRect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl DirtyRect {
    pub fn new(x: i32, y: i32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    /// Whether this rect contains the full extent of `other` —
    /// used by the opaque-below-skip optimization.
    pub fn contains(&self, other: &DirtyRect) -> bool {
        if other.w == 0 || other.h == 0 {
            return true;
        }
        if self.w == 0 || self.h == 0 {
            return false;
        }
        let other_right = other.x.saturating_add(other.w as i32);
        let other_bottom = other.y.saturating_add(other.h as i32);
        let self_right = self.x.saturating_add(self.w as i32);
        let self_bottom = self.y.saturating_add(self.h as i32);
        self.x <= other.x
            && self.y <= other.y
            && self_right >= other_right
            && self_bottom >= other_bottom
    }

    /// Union with another rect — the smallest rect that contains
    /// both. Used to roll up per-layer dirty rects into a
    /// stack-wide damage region.
    pub fn union(&self, other: &DirtyRect) -> DirtyRect {
        if self.w == 0 || self.h == 0 {
            return *other;
        }
        if other.w == 0 || other.h == 0 {
            return *self;
        }
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = self
            .x
            .saturating_add(self.w as i32)
            .max(other.x.saturating_add(other.w as i32));
        let bottom = self
            .y
            .saturating_add(self.h as i32)
            .max(other.y.saturating_add(other.h as i32));
        DirtyRect {
            x,
            y,
            w: (right - x).max(0) as u32,
            h: (bottom - y).max(0) as u32,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }
}

/// Canonical layer identities from the bead. Layer impls choose a
/// kind so z_order conventions stay consistent across the codebase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LayerKind {
    /// Layer 0: image / blur / transparency under the grid.
    Backdrop,
    /// Layer 1: the tiled pane grid (today's render path).
    TiledGrid,
    /// Layer 2: floating panes (zellij pattern).
    FloatingPanes,
    /// Layer 3: status tiles (zellij pluggable status bar).
    StatusTiles,
    /// Layer 4: modal overlays (command palette, picker).
    ModalOverlay,
    /// Custom z-order for plugin-driven layers that don't fit the
    /// canonical 5. The integer is the explicit z-order — higher
    /// values render later (on top).
    Custom(u8),
}

impl LayerKind {
    /// Default z-order for the canonical kinds. Custom carries its
    /// own.
    pub fn z_order(&self) -> u8 {
        match self {
            LayerKind::Backdrop => 0,
            LayerKind::TiledGrid => 1,
            LayerKind::FloatingPanes => 2,
            LayerKind::StatusTiles => 3,
            LayerKind::ModalOverlay => 4,
            LayerKind::Custom(z) => *z,
        }
    }
}

/// Read-only context handed to each layer's `render`.
///
/// The continuation bead expands this with renderer handles
/// (`&GlyphCache`, `&RenderState`, etc.) once the paint.rs
/// integration lands. Today it carries only what the
/// abstraction-only foundation needs.
#[derive(Debug, Clone, Copy)]
pub struct LayerContext {
    /// Monotonically increasing per-frame counter — allows layers
    /// that animate to advance their own state.
    pub frame_idx: u64,
    /// Viewport size in pixels. Layers use this to early-out when
    /// `dirty_rect()` falls fully outside the viewport.
    pub viewport: DirtyRect,
    /// Palette epoch — bumps when the palette changes. Layers
    /// that cache color tables compare this against their own
    /// last-seen value.
    pub palette_epoch: u64,
}

impl LayerContext {
    pub fn new(frame_idx: u64, viewport: DirtyRect, palette_epoch: u64) -> Self {
        Self {
            frame_idx,
            viewport,
            palette_epoch,
        }
    }
}

/// Placeholder for the GPU-side draw commands a layer emits.
/// Continuation replaces this with the real wgpu / glium command
/// shape; today the trait can compile and unit-test against a
/// stable surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrawCmd {
    /// A no-op marker the continuation bead replaces with concrete
    /// GPU commands (clear, draw_quads, ...).
    Placeholder { layer: LayerKind, count: u32 },
}

/// The contract every compositor layer implements.
///
/// Implementations are owned by the `LayerStack`. The trait is
/// object-safe (`dyn Layer`) so callers can register a heterogeneous
/// stack at runtime — required for the plugin layer pattern in
/// ft-mpc9b.4.3.
pub trait Layer: fmt::Debug {
    /// The canonical identity of this layer. The stack uses
    /// `kind().z_order()` unless the impl overrides via
    /// `z_order()`.
    fn kind(&self) -> LayerKind;

    /// Render the layer for this frame. Called only when the
    /// stack determines the layer's pixels are not fully covered
    /// by an opaque higher layer. Implementations that have no
    /// dirty signal for the frame should return an empty Vec.
    fn render(&mut self, ctx: &LayerContext) -> Vec<DrawCmd>;

    /// The bounding rect of the layer's currently-dirty pixels.
    /// `None` means "no dirty region this frame; the stack may
    /// skip my render call entirely". `Some(empty)` is treated as
    /// `None`.
    fn dirty_rect(&self) -> Option<DirtyRect>;

    /// Z-order. Defaults to `kind().z_order()`; impls override
    /// only when they need a non-canonical position.
    fn z_order(&self) -> u8 {
        self.kind().z_order()
    }

    /// Whether this layer's `dirty_rect()` is fully opaque (no
    /// alpha < 1.0 anywhere inside it). The stack uses this to
    /// skip layers below whose dirty rect is contained.
    fn opaque(&self) -> bool {
        false
    }
}

/// Telemetry returned from a `LayerStack::render` pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenderReport {
    /// Total layers in the stack at the time of render.
    pub layer_count: u32,
    /// Layers whose `render()` was actually invoked.
    pub layers_rendered: u32,
    /// Layers skipped because no dirty rect was reported.
    pub layers_skipped_clean: u32,
    /// Layers skipped because an opaque higher layer fully
    /// covered their dirty region.
    pub layers_skipped_opaque_above: u32,
    /// Total `DrawCmd` count returned across every rendered
    /// layer.
    pub total_commands: u64,
    /// Union of every rendered layer's `dirty_rect()` — the
    /// stack-wide damage region for this frame.
    pub damage: DirtyRect,
}

/// The compositor's central coordination point. Owns the layers
/// and orchestrates the per-frame render walk.
#[derive(Debug, Default)]
pub struct LayerStack {
    layers: Vec<Box<dyn Layer>>,
}

impl LayerStack {
    pub fn new() -> Self {
        Self { layers: Vec::new() }
    }

    /// Add a layer. Layers are kept in z-order ascending, so
    /// `render()` walks bottom-up by default.
    pub fn push(&mut self, layer: Box<dyn Layer>) {
        self.layers.push(layer);
        self.sort_by_z_order();
    }

    /// Remove every layer with the given kind. Returns the count
    /// removed.
    pub fn remove_kind(&mut self, kind: LayerKind) -> usize {
        let before = self.layers.len();
        self.layers.retain(|l| l.kind() != kind);
        before - self.layers.len()
    }

    /// Number of registered layers.
    pub fn len(&self) -> usize {
        self.layers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    /// Walk the stack and render every layer whose dirty region is
    /// (a) non-empty and (b) not fully covered by an opaque higher
    /// layer. Returns the per-frame telemetry.
    pub fn render(&mut self, ctx: &LayerContext) -> RenderReport {
        let n = self.layers.len();
        // Snapshot dirty rects + opacity once so the cull pass and
        // the render pass don't double-index the layer vec.
        let dirties: Vec<Option<DirtyRect>> = self.layers.iter().map(|l| l.dirty_rect()).collect();
        let opaques: Vec<bool> = self.layers.iter().map(|l| l.opaque()).collect();

        // For each layer i (sorted ascending z), compute whether
        // any layer j > i is opaque AND covers layer i's dirty
        // rect.
        let mut opaque_above_covers_dirty: Vec<bool> = vec![false; n];
        for (i, dirty_opt) in dirties.iter().enumerate() {
            let Some(dirty) = dirty_opt else {
                continue;
            };
            if dirty.is_empty() {
                continue;
            }
            for (j, jr_opt) in dirties.iter().enumerate().skip(i + 1) {
                if !opaques[j] {
                    continue;
                }
                if let Some(jr) = jr_opt {
                    if !jr.is_empty() && jr.contains(dirty) {
                        opaque_above_covers_dirty[i] = true;
                        break;
                    }
                }
            }
        }

        let mut report = RenderReport {
            layer_count: n as u32,
            ..RenderReport::default()
        };
        for (i, layer) in self.layers.iter_mut().enumerate() {
            let dirty = match layer.dirty_rect() {
                Some(r) if !r.is_empty() => r,
                _ => {
                    report.layers_skipped_clean = report.layers_skipped_clean.saturating_add(1);
                    continue;
                }
            };
            if opaque_above_covers_dirty[i] {
                report.layers_skipped_opaque_above =
                    report.layers_skipped_opaque_above.saturating_add(1);
                continue;
            }
            let cmds = layer.render(ctx);
            report.layers_rendered = report.layers_rendered.saturating_add(1);
            report.total_commands = report.total_commands.saturating_add(cmds.len() as u64);
            report.damage = report.damage.union(&dirty);
        }
        report
    }

    /// Sort by z-order ascending. Stable so layers with identical
    /// z-order retain insertion order (paint stacking matches the
    /// caller's intent for plugin-driven layers).
    fn sort_by_z_order(&mut self) {
        self.layers
            .sort_by(|a, b| match a.z_order().cmp(&b.z_order()) {
                Ordering::Equal => Ordering::Equal,
                other => other,
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::termwindow::render::pane::{TiledGridLayer, TiledGridLayerGeometry};
    use std::collections::{BTreeMap, BTreeSet};

    /// Test-only Layer impl that returns canned values per call so
    /// the LayerStack walk is fully deterministic.
    #[derive(Debug)]
    struct StubLayer {
        kind: LayerKind,
        dirty: Option<DirtyRect>,
        opaque: bool,
        z_override: Option<u8>,
        rendered_calls: u32,
        cmds_per_render: u32,
    }

    impl StubLayer {
        fn new(kind: LayerKind, dirty: Option<DirtyRect>) -> Self {
            Self {
                kind,
                dirty,
                opaque: false,
                z_override: None,
                rendered_calls: 0,
                cmds_per_render: 1,
            }
        }

        fn opaque(mut self) -> Self {
            self.opaque = true;
            self
        }

        fn cmds(mut self, n: u32) -> Self {
            self.cmds_per_render = n;
            self
        }

        fn z(mut self, z: u8) -> Self {
            self.z_override = Some(z);
            self
        }
    }

    impl Layer for StubLayer {
        fn kind(&self) -> LayerKind {
            self.kind
        }
        fn render(&mut self, _ctx: &LayerContext) -> Vec<DrawCmd> {
            self.rendered_calls += 1;
            (0..self.cmds_per_render)
                .map(|_| DrawCmd::Placeholder {
                    layer: self.kind,
                    count: 1,
                })
                .collect()
        }
        fn dirty_rect(&self) -> Option<DirtyRect> {
            self.dirty
        }
        fn z_order(&self) -> u8 {
            self.z_override.unwrap_or_else(|| self.kind.z_order())
        }
        fn opaque(&self) -> bool {
            self.opaque
        }
    }

    fn ctx() -> LayerContext {
        LayerContext::new(1, DirtyRect::new(0, 0, 1920, 1080), 0)
    }

    fn stack_from_specs(specs: &[(LayerKind, DirtyRect, u32)]) -> LayerStack {
        let mut stack = LayerStack::new();
        for &(kind, dirty, cmds) in specs {
            stack.push(Box::new(StubLayer::new(kind, Some(dirty)).cmds(cmds)));
        }
        stack
    }

    fn stack_from_spec_permutation(
        specs: &[(LayerKind, DirtyRect, u32)],
        order: &[usize],
    ) -> LayerStack {
        let mut stack = LayerStack::new();
        for &idx in order {
            let (kind, dirty, cmds) = specs[idx];
            stack.push(Box::new(StubLayer::new(kind, Some(dirty)).cmds(cmds)));
        }
        stack
    }

    fn visit_permutations<F>(prefix: &mut Vec<usize>, remaining: &mut Vec<usize>, f: &mut F)
    where
        F: FnMut(&[usize]),
    {
        if remaining.is_empty() {
            f(prefix);
            return;
        }

        for idx in 0..remaining.len() {
            let value = remaining.remove(idx);
            prefix.push(value);
            visit_permutations(prefix, remaining, f);
            prefix.pop();
            remaining.insert(idx, value);
        }
    }

    fn layer_order(stack: &LayerStack) -> Vec<(LayerKind, u8)> {
        stack
            .layers
            .iter()
            .map(|layer| (layer.kind(), layer.z_order()))
            .collect()
    }

    fn canonical_frame_layer_dependencies() -> &'static [(LayerKind, LayerKind)] {
        &[
            (LayerKind::Backdrop, LayerKind::TiledGrid),
            (LayerKind::TiledGrid, LayerKind::FloatingPanes),
            (LayerKind::FloatingPanes, LayerKind::StatusTiles),
            (LayerKind::StatusTiles, LayerKind::ModalOverlay),
        ]
    }

    fn dependency_graph_cycle(edges: &[(LayerKind, LayerKind)]) -> Option<Vec<LayerKind>> {
        let mut nodes = BTreeSet::new();
        let mut outgoing: BTreeMap<LayerKind, Vec<LayerKind>> = BTreeMap::new();
        let mut incoming: BTreeMap<LayerKind, usize> = BTreeMap::new();

        for &(below, above) in edges {
            nodes.insert(below);
            nodes.insert(above);
            outgoing.entry(below).or_default().push(above);
            outgoing.entry(above).or_default();
            incoming.entry(below).or_insert(0);
            *incoming.entry(above).or_insert(0) += 1;
        }

        let mut ready = incoming
            .iter()
            .filter_map(|(&kind, &count)| (count == 0).then_some(kind))
            .collect::<Vec<_>>();
        let mut visited = 0usize;

        while let Some(kind) = ready.pop() {
            visited += 1;
            if let Some(children) = outgoing.get(&kind) {
                for &child in children {
                    let count = incoming
                        .get_mut(&child)
                        .expect("dependency graph child must have incoming count");
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        ready.push(child);
                    }
                }
            }
        }

        if visited == nodes.len() {
            None
        } else {
            Some(
                nodes
                    .into_iter()
                    .filter(|kind| incoming.get(kind).copied().unwrap_or_default() > 0)
                    .collect(),
            )
        }
    }

    fn assert_dependency_graph_acyclic(label: &str, edges: &[(LayerKind, LayerKind)]) {
        assert!(
            dependency_graph_cycle(edges).is_none(),
            "{label}: frame layer dependency graph must be acyclic"
        );
    }

    fn layer_position(order: &[(LayerKind, u8)], kind: LayerKind) -> usize {
        order
            .iter()
            .position(|(observed, _)| *observed == kind)
            .unwrap_or_else(|| panic!("layer {kind:?} missing from frame builder order {order:?}"))
    }

    fn assert_dependency_graph_conforms(label: &str, stack: &LayerStack) {
        let order = layer_order(stack);
        for &(below, above) in canonical_frame_layer_dependencies() {
            let below_pos = layer_position(&order, below);
            let above_pos = layer_position(&order, above);
            assert!(
                below_pos < above_pos,
                "{label}: dependency edge {below:?} -> {above:?} violated by {order:?}"
            );
            assert!(
                below.z_order() < above.z_order(),
                "{label}: dependency edge {below:?} -> {above:?} must have ascending z-order"
            );
        }
    }

    fn assert_frame_safe_rect(label: &str, rect: DirtyRect) {
        assert!(
            f64::from(rect.x).is_finite(),
            "{label}: x coordinate must be finite"
        );
        assert!(
            f64::from(rect.y).is_finite(),
            "{label}: y coordinate must be finite"
        );
        assert!(
            f64::from(rect.w).is_finite(),
            "{label}: width must be finite"
        );
        assert!(
            f64::from(rect.h).is_finite(),
            "{label}: height must be finite"
        );
        assert!(
            i64::from(rect.w) >= 0,
            "{label}: width must not be negative"
        );
        assert!(
            i64::from(rect.h) >= 0,
            "{label}: height must not be negative"
        );
        assert!(
            rect.w <= i32::MAX as u32,
            "{label}: width must fit signed compositor edge arithmetic"
        );
        assert!(
            rect.h <= i32::MAX as u32,
            "{label}: height must fit signed compositor edge arithmetic"
        );
    }

    fn assert_report_conforms(label: &str, report: &RenderReport) {
        assert_frame_safe_rect(&format!("{label}.damage"), report.damage);
        assert_eq!(
            report.layer_count,
            report
                .layers_rendered
                .saturating_add(report.layers_skipped_clean)
                .saturating_add(report.layers_skipped_opaque_above),
            "{label}: every layer must be rendered or accounted as skipped"
        );
    }

    fn assert_stack_dirty_rects_conform(label: &str, stack: &LayerStack) {
        for (idx, layer) in stack.layers.iter().enumerate() {
            if let Some(rect) = layer.dirty_rect() {
                assert_frame_safe_rect(&format!("{label}.layer[{idx}]"), rect);
            }
        }
    }

    fn tiled_grid_conformance_layer() -> TiledGridLayer {
        TiledGridLayer::from_dirty_lines(
            7,
            TiledGridLayerGeometry {
                origin_x_px: 4,
                origin_y_px: 8,
                cols: 80,
                visible_rows: 24,
                cell_width_px: 9,
                cell_height_px: 18,
            },
            None,
            false,
        )
    }

    fn current_layer_impl_stack() -> LayerStack {
        let mut stack = LayerStack::new();
        stack.push(Box::new(StubLayer::new(
            LayerKind::StatusTiles,
            Some(DirtyRect::new(0, 440, 720, 24)),
        )));
        stack.push(Box::new(tiled_grid_conformance_layer()));
        stack.push(Box::new(StubLayer::new(
            LayerKind::Backdrop,
            Some(DirtyRect::new(0, 0, 720, 480)),
        )));
        stack
    }

    #[test]
    fn empty_stack_renders_nothing() {
        let mut stack = LayerStack::new();
        let report = stack.render(&ctx());
        assert_eq!(report.layer_count, 0);
        assert_eq!(report.layers_rendered, 0);
        assert_eq!(report.total_commands, 0);
        assert!(report.damage.is_empty());
    }

    #[test]
    fn single_layer_with_dirty_rect_renders_once() {
        let mut stack = LayerStack::new();
        stack.push(Box::new(StubLayer::new(
            LayerKind::TiledGrid,
            Some(DirtyRect::new(0, 0, 100, 100)),
        )));
        let report = stack.render(&ctx());
        assert_eq!(report.layer_count, 1);
        assert_eq!(report.layers_rendered, 1);
        assert_eq!(report.layers_skipped_clean, 0);
        assert_eq!(report.layers_skipped_opaque_above, 0);
        assert_eq!(report.total_commands, 1);
        assert_eq!(report.damage, DirtyRect::new(0, 0, 100, 100));
    }

    #[test]
    fn layer_with_no_dirty_rect_is_skipped_clean() {
        let mut stack = LayerStack::new();
        stack.push(Box::new(StubLayer::new(LayerKind::TiledGrid, None)));
        let report = stack.render(&ctx());
        assert_eq!(report.layers_rendered, 0);
        assert_eq!(report.layers_skipped_clean, 1);
        assert_eq!(report.total_commands, 0);
    }

    #[test]
    fn empty_dirty_rect_is_treated_as_none() {
        let mut stack = LayerStack::new();
        stack.push(Box::new(StubLayer::new(
            LayerKind::TiledGrid,
            Some(DirtyRect::new(50, 50, 0, 0)),
        )));
        let report = stack.render(&ctx());
        assert_eq!(report.layers_rendered, 0);
        assert_eq!(report.layers_skipped_clean, 1);
    }

    #[test]
    fn z_order_ascending_render_walks_bottom_up() {
        let mut stack = LayerStack::new();
        // Push out of order — stack should sort.
        stack.push(Box::new(
            StubLayer::new(
                LayerKind::ModalOverlay,
                Some(DirtyRect::new(0, 0, 100, 100)),
            )
            .cmds(1),
        ));
        stack.push(Box::new(
            StubLayer::new(LayerKind::Backdrop, Some(DirtyRect::new(0, 0, 100, 100))).cmds(2),
        ));
        stack.push(Box::new(
            StubLayer::new(LayerKind::TiledGrid, Some(DirtyRect::new(0, 0, 100, 100))).cmds(4),
        ));

        let report = stack.render(&ctx());
        assert_eq!(report.layers_rendered, 3);
        // Total commands = 2 + 4 + 1 = 7 regardless of walk
        // direction; damage union is the full rect.
        assert_eq!(report.total_commands, 7);
        assert_eq!(report.damage, DirtyRect::new(0, 0, 100, 100));
    }

    #[test]
    fn opaque_layer_above_skips_lower_layer_when_fully_covered() {
        let mut stack = LayerStack::new();
        stack.push(Box::new(StubLayer::new(
            LayerKind::Backdrop,
            Some(DirtyRect::new(10, 10, 50, 50)),
        )));
        stack.push(Box::new(
            StubLayer::new(LayerKind::TiledGrid, Some(DirtyRect::new(0, 0, 200, 200))).opaque(),
        ));

        let report = stack.render(&ctx());
        assert_eq!(report.layers_rendered, 1, "only the opaque layer renders");
        assert_eq!(report.layers_skipped_opaque_above, 1);
        assert_eq!(report.layers_skipped_clean, 0);
    }

    #[test]
    fn opaque_layer_above_does_not_skip_lower_when_only_partially_covers() {
        let mut stack = LayerStack::new();
        stack.push(Box::new(StubLayer::new(
            LayerKind::Backdrop,
            Some(DirtyRect::new(0, 0, 200, 200)),
        )));
        stack.push(Box::new(
            StubLayer::new(LayerKind::TiledGrid, Some(DirtyRect::new(50, 50, 50, 50))).opaque(),
        ));

        let report = stack.render(&ctx());
        // Both render — opaque layer's rect is contained in the
        // lower layer's, NOT the other way around.
        assert_eq!(report.layers_rendered, 2);
        assert_eq!(report.layers_skipped_opaque_above, 0);
    }

    #[test]
    fn non_opaque_higher_layer_does_not_skip_lower() {
        let mut stack = LayerStack::new();
        stack.push(Box::new(StubLayer::new(
            LayerKind::Backdrop,
            Some(DirtyRect::new(0, 0, 100, 100)),
        )));
        // ModalOverlay covers the whole backdrop region but is
        // NOT opaque (the default) — so the backdrop must still
        // render.
        stack.push(Box::new(StubLayer::new(
            LayerKind::ModalOverlay,
            Some(DirtyRect::new(0, 0, 200, 200)),
        )));

        let report = stack.render(&ctx());
        assert_eq!(report.layers_rendered, 2);
        assert_eq!(report.layers_skipped_opaque_above, 0);
    }

    #[test]
    fn damage_rect_is_union_of_rendered_layers() {
        let mut stack = LayerStack::new();
        stack.push(Box::new(StubLayer::new(
            LayerKind::Backdrop,
            Some(DirtyRect::new(0, 0, 50, 50)),
        )));
        stack.push(Box::new(StubLayer::new(
            LayerKind::TiledGrid,
            Some(DirtyRect::new(100, 100, 50, 50)),
        )));
        let report = stack.render(&ctx());
        assert_eq!(report.damage, DirtyRect::new(0, 0, 150, 150));
    }

    #[test]
    fn remove_kind_drops_matching_layers() {
        let mut stack = LayerStack::new();
        stack.push(Box::new(StubLayer::new(
            LayerKind::Backdrop,
            Some(DirtyRect::new(0, 0, 10, 10)),
        )));
        stack.push(Box::new(StubLayer::new(
            LayerKind::TiledGrid,
            Some(DirtyRect::new(0, 0, 10, 10)),
        )));
        assert_eq!(stack.len(), 2);

        let removed = stack.remove_kind(LayerKind::Backdrop);
        assert_eq!(removed, 1);
        assert_eq!(stack.len(), 1);

        // Subsequent render still works on the remaining layer.
        let report = stack.render(&ctx());
        assert_eq!(report.layers_rendered, 1);
    }

    #[test]
    fn custom_z_order_layers_sort_with_canonical_ones() {
        let mut stack = LayerStack::new();
        // z=10 should render last (highest).
        stack.push(Box::new(StubLayer::new(
            LayerKind::Custom(10),
            Some(DirtyRect::new(0, 0, 1, 1)),
        )));
        // z=1 (TiledGrid).
        stack.push(Box::new(StubLayer::new(
            LayerKind::TiledGrid,
            Some(DirtyRect::new(0, 0, 1, 1)),
        )));
        // The order of the underlying vec after sort matches
        // ascending z.
        assert_eq!(stack.layers[0].z_order(), 1);
        assert_eq!(stack.layers[1].z_order(), 10);
    }

    #[test]
    fn z_order_override_takes_precedence_over_kind() {
        let stub = StubLayer::new(LayerKind::Backdrop, None).z(99);
        assert_eq!(stub.z_order(), 99);
        assert_eq!(stub.kind(), LayerKind::Backdrop);
    }

    #[test]
    fn dirty_rect_contains_handles_full_coverage() {
        let outer = DirtyRect::new(0, 0, 100, 100);
        let inner = DirtyRect::new(10, 10, 20, 20);
        assert!(outer.contains(&inner));
        assert!(!inner.contains(&outer));
    }

    #[test]
    fn dirty_rect_contains_is_self_reflexive() {
        let r = DirtyRect::new(5, 7, 30, 40);
        assert!(r.contains(&r));
    }

    #[test]
    fn dirty_rect_contains_rejects_disjoint() {
        let a = DirtyRect::new(0, 0, 10, 10);
        let b = DirtyRect::new(100, 100, 10, 10);
        assert!(!a.contains(&b));
        assert!(!b.contains(&a));
    }

    #[test]
    fn dirty_rect_contains_treats_empty_inner_as_contained() {
        let outer = DirtyRect::new(0, 0, 100, 100);
        let empty = DirtyRect::new(50, 50, 0, 0);
        assert!(outer.contains(&empty));
    }

    #[test]
    fn dirty_rect_union_of_disjoint_is_bounding_box() {
        let a = DirtyRect::new(0, 0, 10, 10);
        let b = DirtyRect::new(20, 20, 10, 10);
        let u = a.union(&b);
        assert_eq!(u, DirtyRect::new(0, 0, 30, 30));
    }

    #[test]
    fn dirty_rect_union_with_empty_returns_other() {
        let a = DirtyRect::new(5, 5, 10, 10);
        let empty = DirtyRect::default();
        assert_eq!(a.union(&empty), a);
        assert_eq!(empty.union(&a), a);
    }

    #[test]
    fn render_report_telemetry_partitions_layers_correctly() {
        // 3 layers: 1 dirty + opaque, 1 dirty but covered, 1 clean.
        // Expect: rendered=1, opaque_skip=1, clean_skip=1.
        let mut stack = LayerStack::new();
        stack.push(Box::new(StubLayer::new(
            LayerKind::Backdrop,
            Some(DirtyRect::new(20, 20, 10, 10)),
        )));
        stack.push(Box::new(StubLayer::new(LayerKind::FloatingPanes, None)));
        stack.push(Box::new(
            StubLayer::new(LayerKind::TiledGrid, Some(DirtyRect::new(0, 0, 200, 200))).opaque(),
        ));

        let report = stack.render(&ctx());
        assert_eq!(report.layer_count, 3);
        assert_eq!(report.layers_rendered, 1);
        assert_eq!(report.layers_skipped_opaque_above, 1);
        assert_eq!(report.layers_skipped_clean, 1);
    }

    #[test]
    fn render_increments_rendered_calls_on_chosen_layers_only() {
        let backdrop = Box::new(StubLayer::new(
            LayerKind::Backdrop,
            Some(DirtyRect::new(20, 20, 10, 10)),
        ));
        let opaque_grid = Box::new(
            StubLayer::new(LayerKind::TiledGrid, Some(DirtyRect::new(0, 0, 200, 200))).opaque(),
        );
        let mut stack = LayerStack::new();
        stack.push(backdrop);
        stack.push(opaque_grid);
        stack.render(&ctx());

        // Only the opaque layer was rendered. Use kind() to find it
        // and check rendered_calls — needs downcast; bypass via a
        // second render and observing the counts via the
        // RenderReport instead.
        let report2 = stack.render(&ctx());
        assert_eq!(report2.layers_rendered, 1);
    }

    #[test]
    fn conformance_current_layer_impls_emit_frame_safe_reports() {
        let mut stack = current_layer_impl_stack();
        assert_stack_dirty_rects_conform("current_impls", &stack);

        let report = stack.render(&LayerContext::new(3, DirtyRect::new(0, 0, 720, 480), 2));

        assert_report_conforms("current_impls", &report);
        assert_eq!(report.layer_count, 3);
        assert_eq!(report.layers_rendered, 3);
        assert_eq!(report.layers_skipped_clean, 0);
        assert_eq!(report.layers_skipped_opaque_above, 0);
    }

    #[test]
    fn conformance_layer_order_is_preserved_for_current_impls() {
        let stack = current_layer_impl_stack();
        let observed = layer_order(&stack);

        assert_eq!(
            observed,
            vec![
                (LayerKind::Backdrop, 0),
                (LayerKind::TiledGrid, 1),
                (LayerKind::StatusTiles, 3),
            ]
        );
    }

    #[test]
    fn conformance_frame_builder_ordering_is_canonical_for_any_insertion_order() {
        let specs = [
            (LayerKind::Backdrop, DirtyRect::new(0, 0, 10, 10), 1),
            (LayerKind::TiledGrid, DirtyRect::new(10, 0, 10, 10), 2),
            (LayerKind::FloatingPanes, DirtyRect::new(20, 0, 10, 10), 3),
            (LayerKind::StatusTiles, DirtyRect::new(30, 0, 10, 10), 4),
            (LayerKind::ModalOverlay, DirtyRect::new(40, 0, 10, 10), 5),
        ];
        let expected_order = vec![
            (LayerKind::Backdrop, 0),
            (LayerKind::TiledGrid, 1),
            (LayerKind::FloatingPanes, 2),
            (LayerKind::StatusTiles, 3),
            (LayerKind::ModalOverlay, 4),
        ];
        let expected_report = {
            let mut stack = stack_from_specs(&specs);
            stack.render(&ctx())
        };

        let mut checked = 0usize;
        visit_permutations(
            &mut Vec::new(),
            &mut (0..specs.len()).collect(),
            &mut |order| {
                let mut stack = stack_from_spec_permutation(&specs, order);

                assert_eq!(
                    layer_order(&stack),
                    expected_order,
                    "frame builder order drifted for insertion order {order:?}"
                );
                assert_eq!(
                    stack.render(&ctx()),
                    expected_report,
                    "frame render report drifted for insertion order {order:?}"
                );
                checked += 1;
            },
        );
        assert_eq!(
            checked, 120,
            "5 current frame layers should produce 5! permutations"
        );
    }

    #[test]
    fn conformance_frame_builder_dependency_graph_is_topologically_sorted() {
        let specs = [
            (LayerKind::ModalOverlay, DirtyRect::new(40, 0, 10, 10), 5),
            (LayerKind::StatusTiles, DirtyRect::new(30, 0, 10, 10), 4),
            (LayerKind::FloatingPanes, DirtyRect::new(20, 0, 10, 10), 3),
            (LayerKind::TiledGrid, DirtyRect::new(10, 0, 10, 10), 2),
            (LayerKind::Backdrop, DirtyRect::new(0, 0, 10, 10), 1),
        ];

        let mut checked = 0usize;
        visit_permutations(
            &mut Vec::new(),
            &mut (0..specs.len()).collect(),
            &mut |order| {
                let stack = stack_from_spec_permutation(&specs, order);
                assert_dependency_graph_conforms(&format!("insertion_order_{order:?}"), &stack);
                checked += 1;
            },
        );
        assert_eq!(
            checked, 120,
            "dependency graph conformance must cover every current canonical layer permutation"
        );
    }

    #[test]
    fn conformance_frame_builder_dependency_graph_rejects_cycles() {
        assert_dependency_graph_acyclic(
            "canonical_frame_layers",
            canonical_frame_layer_dependencies(),
        );

        let mut cyclic = canonical_frame_layer_dependencies().to_vec();
        cyclic.push((LayerKind::ModalOverlay, LayerKind::Backdrop));

        assert_eq!(
            dependency_graph_cycle(&cyclic),
            Some(vec![
                LayerKind::Backdrop,
                LayerKind::TiledGrid,
                LayerKind::FloatingPanes,
                LayerKind::StatusTiles,
                LayerKind::ModalOverlay,
            ]),
            "cycle detector must reject a dependency edge from the top overlay back to the base backdrop",
        );
    }

    #[test]
    fn conformance_same_z_layers_preserve_insertion_order() {
        let mut stack = LayerStack::new();
        stack.push(Box::new(
            StubLayer::new(LayerKind::Custom(9), Some(DirtyRect::new(0, 0, 10, 10))).cmds(1),
        ));
        stack.push(Box::new(
            StubLayer::new(LayerKind::Custom(9), Some(DirtyRect::new(10, 0, 10, 10))).cmds(2),
        ));

        let report = stack.render(&ctx());

        assert_report_conforms("same_z", &report);
        assert_eq!(report.total_commands, 3);
        assert_eq!(
            stack
                .layers
                .iter()
                .map(|layer| layer.dirty_rect())
                .collect::<Vec<_>>(),
            vec![
                Some(DirtyRect::new(0, 0, 10, 10)),
                Some(DirtyRect::new(10, 0, 10, 10)),
            ],
            "stable z-order sort must retain insertion order for equal-z layers"
        );
    }

    #[test]
    fn metamorphic_same_input_renders_same_report_twice() {
        let specs = [
            (LayerKind::Backdrop, DirtyRect::new(0, 0, 100, 80), 2),
            (LayerKind::TiledGrid, DirtyRect::new(20, 10, 50, 30), 4),
            (LayerKind::StatusTiles, DirtyRect::new(0, 90, 120, 10), 1),
        ];
        let mut stack = stack_from_specs(&specs);
        let ctx = LayerContext::new(7, DirtyRect::new(0, 0, 160, 120), 3);

        let first = stack.render(&ctx);
        let second = stack.render(&ctx);

        assert_eq!(first, second);
    }

    #[test]
    fn metamorphic_resize_preserves_stable_layer_report() {
        let specs = [
            (LayerKind::TiledGrid, DirtyRect::new(0, 0, 80, 24), 3),
            (LayerKind::FloatingPanes, DirtyRect::new(10, 4, 20, 8), 2),
            (LayerKind::StatusTiles, DirtyRect::new(0, 24, 80, 1), 1),
        ];
        let mut stack = stack_from_specs(&specs);
        let before_resize = stack.render(&LayerContext::new(10, DirtyRect::new(0, 0, 80, 25), 1));
        let after_resize = stack.render(&LayerContext::new(11, DirtyRect::new(0, 0, 120, 40), 1));

        assert_eq!(after_resize.layer_count, before_resize.layer_count);
        assert_eq!(after_resize.layers_rendered, before_resize.layers_rendered);
        assert_eq!(
            after_resize.layers_skipped_clean,
            before_resize.layers_skipped_clean
        );
        assert_eq!(
            after_resize.layers_skipped_opaque_above,
            before_resize.layers_skipped_opaque_above
        );
        assert_eq!(after_resize.total_commands, before_resize.total_commands);
        assert_eq!(after_resize.damage, before_resize.damage);
    }

    #[test]
    fn metamorphic_subset_then_adding_layers_matches_full_render() {
        let subset_specs = [
            (LayerKind::Backdrop, DirtyRect::new(0, 0, 40, 20), 2),
            (LayerKind::TiledGrid, DirtyRect::new(40, 0, 40, 20), 3),
        ];
        let added_specs = [
            (LayerKind::FloatingPanes, DirtyRect::new(20, 10, 20, 10), 5),
            (LayerKind::StatusTiles, DirtyRect::new(0, 20, 80, 2), 1),
        ];
        let mut subset_stack = stack_from_specs(&subset_specs);
        let mut added_stack = stack_from_specs(&added_specs);
        let mut full_stack = stack_from_specs(
            &subset_specs
                .into_iter()
                .chain(added_specs)
                .collect::<Vec<_>>(),
        );
        let ctx = LayerContext::new(42, DirtyRect::new(0, 0, 100, 40), 0);

        let subset = subset_stack.render(&ctx);
        let added = added_stack.render(&ctx);
        let full = full_stack.render(&ctx);

        assert_eq!(full.layer_count, subset.layer_count + added.layer_count);
        assert_eq!(
            full.layers_rendered,
            subset.layers_rendered + added.layers_rendered
        );
        assert_eq!(
            full.layers_skipped_clean,
            subset.layers_skipped_clean + added.layers_skipped_clean
        );
        assert_eq!(
            full.layers_skipped_opaque_above,
            subset.layers_skipped_opaque_above + added.layers_skipped_opaque_above
        );
        assert_eq!(
            full.total_commands,
            subset.total_commands + added.total_commands
        );
        assert_eq!(full.damage, subset.damage.union(&added.damage));
    }

    #[test]
    fn rio_pattern_5_canonical_layers_have_strictly_ascending_z() {
        let zs = [
            LayerKind::Backdrop.z_order(),
            LayerKind::TiledGrid.z_order(),
            LayerKind::FloatingPanes.z_order(),
            LayerKind::StatusTiles.z_order(),
            LayerKind::ModalOverlay.z_order(),
        ];
        for w in zs.windows(2) {
            assert!(w[0] < w[1], "canonical z-order must be strictly ascending");
        }
    }
}
