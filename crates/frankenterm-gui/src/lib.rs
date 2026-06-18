#![cfg_attr(not(feature = "headless-render"), allow(dead_code))]
// Keep this in sync with Cargo.toml: the vendored GUI crate is not yet a
// pedantic-clean primary lint target.
#![allow(clippy::all, clippy::pedantic, clippy::nursery)]

pub mod accessibility_preferences;
pub mod adaptive_fps_loop;
pub mod floating_panes;
pub mod gpu_regression;
pub mod gpu_regression_fuzz;
pub mod input_loop;
pub mod osc8_gui;
pub mod plugins;
pub mod renderer_slo;
pub mod rollout_env;

/// Keep drag-selection geometry alive while live output dirties rows under it.
///
/// This lives in the library crate so the binary-owned `termwindow` selection
/// lifecycle can keep a small pure predicate under normal `cargo test --lib`
/// coverage.
pub fn should_preserve_dirty_selection_during_mouse_drag(
    active_selection_drag_pane_id: Option<usize>,
    captured_pane_id: Option<usize>,
    left_mouse_button_down: bool,
    pane_id: usize,
) -> bool {
    left_mouse_button_down
        && active_selection_drag_pane_id == Some(pane_id)
        && captured_pane_id == Some(pane_id)
}

/// Build an exclusive stable-row range from a top row and visible row count.
///
/// Binary-owned render code uses this to avoid wrapping stable-row arithmetic
/// when a viewport is near the representable row boundary.
pub fn checked_stable_row_range_from_top(
    top: wezterm_term::StableRowIndex,
    row_count: usize,
) -> Option<std::ops::Range<wezterm_term::StableRowIndex>> {
    let row_count =
        <wezterm_term::StableRowIndex as std::convert::TryFrom<usize>>::try_from(row_count).ok()?;
    let end = top.checked_add(row_count)?;
    Some(top..end)
}

pub mod glyph_quad_staging {
    use std::sync::LazyLock;

    pub const VERTICES_PER_GLYPH_QUAD: usize = 4;

    const V_TOP_LEFT: usize = 0;
    const V_TOP_RIGHT: usize = 1;
    const V_BOT_LEFT: usize = 2;
    const V_BOT_RIGHT: usize = 3;
    const GLYPH_QUAD_HAS_COLOR: f32 = 0.0;
    const COLOR_GLYPH_QUAD_HAS_COLOR: f32 = 1.0;
    const FT_DISABLE_MOONSHOT_INSTANCED_GLYPH_QUADS: &str =
        "FT_DISABLE_MOONSHOT_INSTANCED_GLYPH_QUADS";
    const FT_MOONSHOT_INSTANCED_GLYPH_QUADS: &str = "FT_MOONSHOT_INSTANCED_GLYPH_QUADS";

    static MOONSHOT_INSTANCED_GLYPH_QUADS_ENABLED: LazyLock<bool> = LazyLock::new(|| {
        if std::env::var_os(FT_DISABLE_MOONSHOT_INSTANCED_GLYPH_QUADS).is_some() {
            return false;
        }

        cfg!(feature = "headless-render")
            || std::env::var_os(FT_MOONSHOT_INSTANCED_GLYPH_QUADS).is_some()
    });

    #[must_use]
    pub fn moonshot_instanced_glyph_quads_enabled() -> bool {
        *MOONSHOT_INSTANCED_GLYPH_QUADS_ENABLED
    }

    #[repr(C)]
    #[derive(Copy, Clone, Default, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
    pub struct GlyphQuadStagingVertex {
        pub position: [f32; 2],
        pub tex: [f32; 2],
        pub fg_color: [f32; 4],
        pub alt_color: [f32; 4],
        pub hsv: [f32; 3],
        pub has_color: f32,
        pub mix_value: f32,
    }

    #[derive(Debug, Clone, Copy, PartialEq)]
    pub struct GlyphQuadStagingInstance {
        pub position: [f32; 4],
        pub tex: [f32; 4],
        pub fg_color: [f32; 4],
        pub alt_color: [f32; 4],
        pub hsv: [f32; 3],
        pub has_color: f32,
        pub mix_value: f32,
    }

    impl GlyphQuadStagingInstance {
        #[must_use]
        pub fn new(
            position: [f32; 4],
            tex: [f32; 4],
            fg_color: [f32; 4],
            alt_color: [f32; 4],
            hsv: [f32; 3],
            has_color: bool,
            mix_value: f32,
        ) -> Self {
            Self {
                position,
                tex,
                fg_color,
                alt_color,
                hsv,
                has_color: if has_color {
                    COLOR_GLYPH_QUAD_HAS_COLOR
                } else {
                    GLYPH_QUAD_HAS_COLOR
                },
                mix_value,
            }
        }
    }

    #[derive(Debug, Clone, Copy)]
    pub struct GlyphQuadSoaBuffers<'a> {
        pub positions: &'a [[f32; 4]],
        pub tex_rects: &'a [[f32; 4]],
        pub fg_colors: &'a [[f32; 4]],
        pub alt_colors: &'a [[f32; 4]],
        pub hsv: &'a [[f32; 3]],
        pub has_color: &'a [f32],
        pub mix_values: &'a [f32],
    }

    impl GlyphQuadSoaBuffers<'_> {
        #[must_use]
        pub fn len(&self) -> usize {
            self.positions.len()
        }

        #[must_use]
        pub fn is_empty(&self) -> bool {
            self.positions.is_empty()
        }

        fn assert_consistent_lengths(&self) {
            let len = self.len();
            assert_eq!(self.tex_rects.len(), len, "tex rect staging length");
            assert_eq!(self.fg_colors.len(), len, "fg color staging length");
            assert_eq!(self.alt_colors.len(), len, "alt color staging length");
            assert_eq!(self.hsv.len(), len, "hsv staging length");
            assert_eq!(self.has_color.len(), len, "has-color staging length");
            assert_eq!(self.mix_values.len(), len, "mix-value staging length");
        }

        fn instance_at(&self, idx: usize) -> GlyphQuadStagingInstance {
            GlyphQuadStagingInstance {
                position: self.positions[idx],
                tex: self.tex_rects[idx],
                fg_color: self.fg_colors[idx],
                alt_color: self.alt_colors[idx],
                hsv: self.hsv[idx],
                has_color: self.has_color[idx],
                mix_value: self.mix_values[idx],
            }
        }
    }

    #[must_use]
    pub fn expand_glyph_quad_instance(
        instance: GlyphQuadStagingInstance,
    ) -> [GlyphQuadStagingVertex; VERTICES_PER_GLYPH_QUAD] {
        let [left, top, right, bottom] = instance.position;
        let [tex_left, tex_right, tex_top, tex_bottom] = instance.tex;
        let mut vertices = [GlyphQuadStagingVertex {
            fg_color: instance.fg_color,
            alt_color: instance.alt_color,
            hsv: instance.hsv,
            has_color: instance.has_color,
            mix_value: instance.mix_value,
            ..GlyphQuadStagingVertex::default()
        }; VERTICES_PER_GLYPH_QUAD];

        vertices[V_TOP_LEFT].position = [left, top];
        vertices[V_TOP_RIGHT].position = [right, top];
        vertices[V_BOT_LEFT].position = [left, bottom];
        vertices[V_BOT_RIGHT].position = [right, bottom];

        vertices[V_TOP_LEFT].tex = [tex_left, tex_top];
        vertices[V_TOP_RIGHT].tex = [tex_right, tex_top];
        vertices[V_BOT_LEFT].tex = [tex_left, tex_bottom];
        vertices[V_BOT_RIGHT].tex = [tex_right, tex_bottom];

        vertices
    }

    #[must_use]
    pub fn aos_glyph_quad_vertices(
        instance: GlyphQuadStagingInstance,
    ) -> [GlyphQuadStagingVertex; VERTICES_PER_GLYPH_QUAD] {
        let [left, top, right, bottom] = instance.position;
        let mut vertices = [GlyphQuadStagingVertex::default(); VERTICES_PER_GLYPH_QUAD];
        let mut quad = AosGlyphQuad {
            vertices: &mut vertices,
        };

        quad.set_position(left, top, right, bottom);
        quad.set_fg_color(instance.fg_color);
        quad.set_alt_color_and_mix_value(instance.alt_color, instance.mix_value);
        quad.set_texture(instance.tex);
        quad.set_hsv(instance.hsv);
        quad.set_has_color_impl(instance.has_color);

        vertices
    }

    pub fn visit_expanded_glyph_quad_soa_vertices(
        buffers: GlyphQuadSoaBuffers<'_>,
        mut visit: impl FnMut(GlyphQuadStagingVertex),
    ) {
        buffers.assert_consistent_lengths();
        for idx in 0..buffers.len() {
            for vertex in expand_glyph_quad_instance(buffers.instance_at(idx)) {
                visit(vertex);
            }
        }
    }

    #[must_use]
    pub fn expand_glyph_quad_soa_buffers(
        buffers: GlyphQuadSoaBuffers<'_>,
    ) -> Vec<GlyphQuadStagingVertex> {
        let mut vertices = Vec::with_capacity(buffers.len() * VERTICES_PER_GLYPH_QUAD);
        visit_expanded_glyph_quad_soa_vertices(buffers, |vertex| vertices.push(vertex));
        vertices
    }

    #[must_use]
    pub fn glyph_quad_soa_staging_matches_aos_vertices(
        buffers: GlyphQuadSoaBuffers<'_>,
        expected_aos_vertices: &[GlyphQuadStagingVertex],
    ) -> bool {
        let actual_vertices = expand_glyph_quad_soa_buffers(buffers);
        bytemuck::cast_slice::<GlyphQuadStagingVertex, u8>(&actual_vertices)
            == bytemuck::cast_slice::<GlyphQuadStagingVertex, u8>(expected_aos_vertices)
    }

    struct AosGlyphQuad<'a> {
        vertices: &'a mut [GlyphQuadStagingVertex; VERTICES_PER_GLYPH_QUAD],
    }

    impl AosGlyphQuad<'_> {
        fn set_position(&mut self, left: f32, top: f32, right: f32, bottom: f32) {
            self.vertices[V_TOP_LEFT].position = [left, top];
            self.vertices[V_TOP_RIGHT].position = [right, top];
            self.vertices[V_BOT_LEFT].position = [left, bottom];
            self.vertices[V_BOT_RIGHT].position = [right, bottom];
        }

        fn set_texture(&mut self, tex: [f32; 4]) {
            let [x1, x2, y1, y2] = tex;
            self.vertices[V_TOP_LEFT].tex = [x1, y1];
            self.vertices[V_TOP_RIGHT].tex = [x2, y1];
            self.vertices[V_BOT_LEFT].tex = [x1, y2];
            self.vertices[V_BOT_RIGHT].tex = [x2, y2];
        }

        fn set_fg_color(&mut self, color: [f32; 4]) {
            for vertex in self.vertices.iter_mut() {
                vertex.fg_color = color;
            }
            self.set_alt_color_and_mix_value(color, 0.0);
        }

        fn set_alt_color_and_mix_value(&mut self, color: [f32; 4], mix_value: f32) {
            for vertex in self.vertices.iter_mut() {
                vertex.alt_color = color;
                vertex.mix_value = mix_value;
            }
        }

        fn set_hsv(&mut self, hsv: [f32; 3]) {
            for vertex in self.vertices.iter_mut() {
                vertex.hsv = hsv;
            }
        }

        fn set_has_color_impl(&mut self, has_color: f32) {
            for vertex in self.vertices.iter_mut() {
                vertex.has_color = has_color;
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{
            GlyphQuadSoaBuffers, GlyphQuadStagingInstance, GlyphQuadStagingVertex,
            VERTICES_PER_GLYPH_QUAD, aos_glyph_quad_vertices, expand_glyph_quad_instance,
            expand_glyph_quad_soa_buffers, glyph_quad_soa_staging_matches_aos_vertices,
        };

        fn vertex_bytes(vertices: &[GlyphQuadStagingVertex]) -> &[u8] {
            bytemuck::cast_slice(vertices)
        }

        fn screen_line_strips() -> [GlyphQuadStagingInstance; 2] {
            [
                GlyphQuadStagingInstance::new(
                    [-240.0, -18.5, -232.0, -2.5],
                    [0.125, 0.1875, 0.25, 0.375],
                    [0.2, 0.4, 0.8, 1.0],
                    [0.9, 0.6, 0.1, 0.75],
                    [0.9, 0.8, 0.7],
                    false,
                    0.375,
                ),
                GlyphQuadStagingInstance::new(
                    [-232.0, -18.5, -224.0, -2.5],
                    [0.5, 0.5625, 0.625, 0.75],
                    [1.0, 0.25, 0.15, 0.95],
                    [0.05, 0.6, 0.7, 0.8],
                    [1.0, 1.0, 1.0],
                    true,
                    0.625,
                ),
            ]
        }

        fn buffers_from_parts<'a>(
            positions: &'a [[f32; 4]],
            tex_rects: &'a [[f32; 4]],
            fg_colors: &'a [[f32; 4]],
            alt_colors: &'a [[f32; 4]],
            hsv: &'a [[f32; 3]],
            has_color: &'a [f32],
            mix_values: &'a [f32],
        ) -> GlyphQuadSoaBuffers<'a> {
            GlyphQuadSoaBuffers {
                positions,
                tex_rects,
                fg_colors,
                alt_colors,
                hsv,
                has_color,
                mix_values,
            }
        }

        #[test]
        fn soa_glyph_staging_matches_aos_quad_bytes_for_screen_line_strips() {
            let strips = screen_line_strips();
            let mut expected_vertices = Vec::with_capacity(strips.len() * VERTICES_PER_GLYPH_QUAD);

            let mut positions = Vec::with_capacity(strips.len());
            let mut tex_rects = Vec::with_capacity(strips.len());
            let mut fg_colors = Vec::with_capacity(strips.len());
            let mut alt_colors = Vec::with_capacity(strips.len());
            let mut hsv = Vec::with_capacity(strips.len());
            let mut has_color = Vec::with_capacity(strips.len());
            let mut mix_values = Vec::with_capacity(strips.len());

            for strip in strips {
                let aos_vertices = aos_glyph_quad_vertices(strip);
                let soa_vertices = expand_glyph_quad_instance(strip);
                assert_eq!(vertex_bytes(&soa_vertices), vertex_bytes(&aos_vertices));

                expected_vertices.extend_from_slice(&aos_vertices);
                positions.push(strip.position);
                tex_rects.push(strip.tex);
                fg_colors.push(strip.fg_color);
                alt_colors.push(strip.alt_color);
                hsv.push(strip.hsv);
                has_color.push(strip.has_color);
                mix_values.push(strip.mix_value);
            }

            let buffers = buffers_from_parts(
                &positions,
                &tex_rects,
                &fg_colors,
                &alt_colors,
                &hsv,
                &has_color,
                &mix_values,
            );
            let actual_vertices = expand_glyph_quad_soa_buffers(buffers);

            assert_eq!(
                vertex_bytes(&actual_vertices),
                vertex_bytes(&expected_vertices)
            );
            assert!(glyph_quad_soa_staging_matches_aos_vertices(
                buffers,
                &expected_vertices
            ));

            let mut changed_tex_rects = tex_rects.clone();
            changed_tex_rects[1][0] += 0.03125;
            assert!(!glyph_quad_soa_staging_matches_aos_vertices(
                buffers_from_parts(
                    &positions,
                    &changed_tex_rects,
                    &fg_colors,
                    &alt_colors,
                    &hsv,
                    &has_color,
                    &mix_values,
                ),
                &expected_vertices,
            ));

            let mut changed_fg_colors = fg_colors.clone();
            changed_fg_colors[0][2] += 0.125;
            assert!(!glyph_quad_soa_staging_matches_aos_vertices(
                buffers_from_parts(
                    &positions,
                    &tex_rects,
                    &changed_fg_colors,
                    &alt_colors,
                    &hsv,
                    &has_color,
                    &mix_values,
                ),
                &expected_vertices,
            ));

            let mut reordered_positions = positions.clone();
            reordered_positions.swap(0, 1);
            assert!(!glyph_quad_soa_staging_matches_aos_vertices(
                buffers_from_parts(
                    &reordered_positions,
                    &tex_rects,
                    &fg_colors,
                    &alt_colors,
                    &hsv,
                    &has_color,
                    &mix_values,
                ),
                &expected_vertices,
            ));
        }
    }
}

#[allow(unexpected_cfgs)]
pub mod glyph_run_interning {
    use ahash::{AHashMap, AHasher};
    use std::hash::{Hash, Hasher};
    use std::rc::Rc;
    use std::sync::LazyLock;

    const SHAPED_RUN_INTERNER_MAX_ENTRIES: usize = 8192;
    const SHAPED_RUN_INTERNER_MAX_COLLISIONS: usize = 4;

    #[cfg(ft_disable_glyph_run_interning)]
    const GLYPH_RUN_INTERNING_CFG_ENABLED: bool = false;

    #[cfg(not(ft_disable_glyph_run_interning))]
    const GLYPH_RUN_INTERNING_CFG_ENABLED: bool = true;

    static GLYPH_RUN_INTERNING_ENV_ENABLED: LazyLock<bool> =
        LazyLock::new(|| std::env::var_os("FT_DISABLE_GLYPH_RUN_INTERNING").is_none());

    #[must_use]
    #[inline]
    pub fn glyph_run_interning_enabled() -> bool {
        GLYPH_RUN_INTERNING_CFG_ENABLED && *GLYPH_RUN_INTERNING_ENV_ENABLED
    }

    #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
    pub struct GlyphRunProbeGlyph {
        pub glyph_pos: u32,
        pub cluster: u32,
        pub font_idx: usize,
        pub x_advance_bits: u64,
        pub x_offset_bits: u64,
        pub glyph_ptr: usize,
        pub bitmap_pixel_width: u32,
        pub bearing_x_bits: u64,
    }

    #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
    struct GlyphRunProbeKey {
        hash: u64,
        len: usize,
    }

    #[derive(Default)]
    pub struct GlyphRunProbeInterner {
        runs: AHashMap<GlyphRunProbeKey, Vec<Rc<[GlyphRunProbeGlyph]>>>,
        entries: usize,
    }

    impl GlyphRunProbeInterner {
        #[must_use]
        pub fn intern_or_build(
            &mut self,
            glyphs: &[GlyphRunProbeGlyph],
        ) -> Rc<[GlyphRunProbeGlyph]> {
            if !glyph_run_interning_enabled() || glyphs.len() < 2 {
                return Rc::from(glyphs.to_vec().into_boxed_slice());
            }

            let key = glyph_run_probe_key(glyphs);
            if let Some(run) = self.lookup(key, glyphs) {
                return run;
            }

            let run: Rc<[GlyphRunProbeGlyph]> = Rc::from(glyphs.to_vec().into_boxed_slice());
            self.insert(key, Rc::clone(&run));
            run
        }

        fn lookup(
            &self,
            key: GlyphRunProbeKey,
            glyphs: &[GlyphRunProbeGlyph],
        ) -> Option<Rc<[GlyphRunProbeGlyph]>> {
            self.runs
                .get(&key)
                .and_then(|runs| runs.iter().find(|run| run.as_ref() == glyphs).cloned())
        }

        fn insert(&mut self, key: GlyphRunProbeKey, run: Rc<[GlyphRunProbeGlyph]>) {
            if self.entries >= SHAPED_RUN_INTERNER_MAX_ENTRIES {
                self.runs.clear();
                self.entries = 0;
            }

            let bucket = self.runs.entry(key).or_default();
            if bucket.len() >= SHAPED_RUN_INTERNER_MAX_COLLISIONS {
                bucket.swap_remove(0);
                self.entries = self.entries.saturating_sub(1);
            }

            bucket.push(run);
            self.entries += 1;
        }
    }

    #[must_use]
    pub fn glyph_run_probe_iteration(glyphs: &[GlyphRunProbeGlyph], repeats: usize) -> usize {
        let mut interner = GlyphRunProbeInterner::default();
        let mut retained = 0usize;
        for _ in 0..repeats {
            let run = interner.intern_or_build(glyphs);
            retained = retained
                .wrapping_add(run.len())
                .wrapping_add(Rc::strong_count(&run));
        }
        retained
    }

    fn glyph_run_probe_key(glyphs: &[GlyphRunProbeGlyph]) -> GlyphRunProbeKey {
        let mut hasher = AHasher::default();
        glyphs.len().hash(&mut hasher);
        glyphs.hash(&mut hasher);
        GlyphRunProbeKey {
            hash: hasher.finish(),
            len: glyphs.len(),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{
            GlyphRunProbeGlyph, GlyphRunProbeInterner, glyph_run_interning_enabled,
            glyph_run_probe_iteration,
        };
        use std::rc::Rc;

        fn probe_glyphs() -> Vec<GlyphRunProbeGlyph> {
            (0..8)
                .map(|idx| GlyphRunProbeGlyph {
                    glyph_pos: 40 + idx,
                    cluster: idx,
                    font_idx: 1,
                    x_advance_bits: (8.0f64 + f64::from(idx) / 16.0).to_bits(),
                    x_offset_bits: (f64::from(idx) / 32.0).to_bits(),
                    glyph_ptr: 0x1000 + idx as usize * 32,
                    bitmap_pixel_width: 9 + idx,
                    bearing_x_bits: (1.0f64 + f64::from(idx) / 64.0).to_bits(),
                })
                .collect()
        }

        #[test]
        fn glyph_run_probe_hits_when_interning_enabled() {
            assert!(
                glyph_run_interning_enabled(),
                "glyph-run interning must be enabled for the bench gate"
            );
            let glyphs = probe_glyphs();
            let mut interner = GlyphRunProbeInterner::default();

            let miss = interner.intern_or_build(&glyphs);
            let hit = interner.intern_or_build(&glyphs);

            assert!(Rc::ptr_eq(&miss, &hit));
            assert!(glyph_run_probe_iteration(&glyphs, 4) >= glyphs.len());
        }
    }
}

#[doc(hidden)]
pub mod owner_last_guard {
    use std::mem::ManuallyDrop;

    pub struct OwnerLastGuardedMapping<M, S, O> {
        mapping: ManuallyDrop<M>,
        slice: ManuallyDrop<S>,
        owner: ManuallyDrop<O>,
    }

    impl<M, S, O> OwnerLastGuardedMapping<M, S, O> {
        pub fn new(mapping: M, slice: S, owner: O) -> Self {
            Self {
                mapping: ManuallyDrop::new(mapping),
                slice: ManuallyDrop::new(slice),
                owner: ManuallyDrop::new(owner),
            }
        }

        pub fn mapping_mut(&mut self) -> &mut M {
            &mut self.mapping
        }
    }

    impl<M, S, O> Drop for OwnerLastGuardedMapping<M, S, O> {
        fn drop(&mut self) {
            unsafe {
                // SAFETY: each field is wrapped in ManuallyDrop and is dropped exactly
                // once here, in dependency order, so derived views go away before owner.
                ManuallyDrop::drop(&mut self.mapping);
                ManuallyDrop::drop(&mut self.slice);
                ManuallyDrop::drop(&mut self.owner);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::OwnerLastGuardedMapping;
        use std::cell::RefCell;
        use std::rc::Rc;

        struct DropProbe {
            name: &'static str,
            log: Rc<RefCell<Vec<&'static str>>>,
        }

        impl DropProbe {
            fn new(name: &'static str, log: &Rc<RefCell<Vec<&'static str>>>) -> Self {
                Self {
                    name,
                    log: Rc::clone(log),
                }
            }
        }

        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.log.borrow_mut().push(self.name);
            }
        }

        #[test]
        fn drops_derived_mapping_and_slice_before_owner() {
            let log = Rc::new(RefCell::new(Vec::new()));

            {
                let mut guard = OwnerLastGuardedMapping::new(
                    DropProbe::new("mapping", &log),
                    DropProbe::new("slice", &log),
                    DropProbe::new("owner", &log),
                );

                assert_eq!(guard.mapping_mut().name, "mapping");
            }

            assert_eq!(&*log.borrow(), &["mapping", "slice", "owner"]);
        }
    }
}

#[cfg(test)]
mod selection_lifecycle_tests {
    use super::{
        checked_stable_row_range_from_top, should_preserve_dirty_selection_during_mouse_drag,
    };
    use wezterm_term::StableRowIndex;

    #[test]
    fn dirty_selection_is_preserved_only_for_active_left_drag_on_same_pane() {
        assert!(should_preserve_dirty_selection_during_mouse_drag(
            Some(7),
            Some(7),
            true,
            7,
        ));
        assert!(!should_preserve_dirty_selection_during_mouse_drag(
            None,
            Some(7),
            true,
            7,
        ));
        assert!(!should_preserve_dirty_selection_during_mouse_drag(
            Some(7),
            None,
            true,
            7,
        ));
        assert!(!should_preserve_dirty_selection_during_mouse_drag(
            Some(7),
            Some(8),
            true,
            7,
        ));
        assert!(!should_preserve_dirty_selection_during_mouse_drag(
            Some(8),
            Some(7),
            true,
            7,
        ));
        assert!(!should_preserve_dirty_selection_during_mouse_drag(
            Some(7),
            Some(7),
            false,
            7,
        ));
    }

    #[test]
    fn checked_stable_row_range_from_top_rejects_unrepresentable_ranges() {
        assert_eq!(checked_stable_row_range_from_top(10, 3), Some(10..13));
        assert_eq!(
            checked_stable_row_range_from_top(StableRowIndex::MAX, 1),
            None
        );
        assert_eq!(checked_stable_row_range_from_top(0, usize::MAX), None);
    }
}

pub mod command_rules {
    use config::keyassignment::KeyAssignment::*;
    use config::keyassignment::*;
    use config::window::WindowLevel;
    use mux::domain::DomainState;
    use ordered_float::NotNan;
    use window::Modifiers;

    pub const PANE_SELECT_DEFAULT_MODES: [PaneSelectMode; 5] = [
        PaneSelectMode::Activate,
        PaneSelectMode::SwapWithActive,
        PaneSelectMode::SwapWithActiveKeepFocus,
        PaneSelectMode::MoveToNewTab,
        PaneSelectMode::MoveToNewWindow,
    ];

    pub fn domain_detach_command_is_available(
        name: &str,
        state: DomainState,
        detachable: bool,
    ) -> bool {
        state == DomainState::Attached && detachable && name != "local"
    }

    pub fn pane_select_default_keys(mode: PaneSelectMode) -> Vec<(Modifiers, String)> {
        match mode {
            PaneSelectMode::Activate => {
                vec![(Modifiers::CTRL.union(Modifiers::SHIFT), "9".into())]
            }
            PaneSelectMode::SwapWithActive => {
                vec![(Modifiers::CTRL.union(Modifiers::SHIFT), "0".into())]
            }
            PaneSelectMode::SwapWithActiveKeepFocus => {
                vec![(Modifiers::SUPER.union(Modifiers::SHIFT), "0".into())]
            }
            PaneSelectMode::MoveToNewTab => {
                vec![(Modifiers::CTRL.union(Modifiers::SHIFT), "t".into())]
            }
            PaneSelectMode::MoveToNewWindow => {
                vec![(Modifiers::CTRL.union(Modifiers::SHIFT), "y".into())]
            }
        }
    }

    pub fn pane_select_default_action(mode: PaneSelectMode) -> KeyAssignment {
        PaneSelect(PaneSelectArguments {
            alphabet: String::new(),
            mode,
            show_pane_ids: false,
        })
    }

    /// Returns a list of key assignment actions that should be included in
    /// the default key assignments and command palette.
    pub fn compute_default_actions() -> Vec<KeyAssignment> {
        // These are ordered by their position within the various menus.
        vec![
            // ----------------- WezTerm
            ReloadConfiguration,
            #[cfg(target_os = "macos")]
            HideApplication,
            #[cfg(target_os = "macos")]
            QuitApplication,
            // ----------------- Shell
            SpawnTab(SpawnTabDomain::CurrentPaneDomain),
            SpawnWindow,
            SplitVertical(SpawnCommand {
                domain: SpawnTabDomain::CurrentPaneDomain,
                ..Default::default()
            }),
            SplitHorizontal(SpawnCommand {
                domain: SpawnTabDomain::CurrentPaneDomain,
                ..Default::default()
            }),
            CloseCurrentTab { confirm: true },
            CloseCurrentPane { confirm: true },
            ResetTerminal,
            // ----------------- Edit
            #[cfg(not(target_os = "macos"))]
            PasteFrom(ClipboardPasteSource::PrimarySelection),
            #[cfg(not(target_os = "macos"))]
            CopyTo(ClipboardCopyDestination::PrimarySelection),
            CopyTo(ClipboardCopyDestination::Clipboard),
            PasteFrom(ClipboardPasteSource::Clipboard),
            ClearScrollback(ScrollbackEraseMode::ScrollbackOnly),
            ClearScrollback(ScrollbackEraseMode::ScrollbackAndViewport),
            QuickSelect,
            CharSelect(CharSelectArguments::default()),
            ActivateCopyMode,
            ClearKeyTableStack,
            ActivateCommandPalette,
            // ----------------- View
            DecreaseFontSize,
            IncreaseFontSize,
            ResetFontSize,
            ResetFontAndWindowSize,
            ScrollByPage(NotNan::new(-1.0).unwrap()),
            ScrollByPage(NotNan::new(1.0).unwrap()),
            ScrollToTop,
            ScrollToBottom,
            // ----------------- Window
            ToggleFullScreen,
            ToggleAlwaysOnTop,
            ToggleAlwaysOnBottom,
            SetWindowLevel(WindowLevel::AlwaysOnBottom),
            SetWindowLevel(WindowLevel::Normal),
            SetWindowLevel(WindowLevel::AlwaysOnTop),
            Hide,
            Search(Pattern::CurrentSelectionOrEmptyString),
            pane_select_default_action(PaneSelectMode::Activate),
            pane_select_default_action(PaneSelectMode::SwapWithActive),
            pane_select_default_action(PaneSelectMode::SwapWithActiveKeepFocus),
            pane_select_default_action(PaneSelectMode::MoveToNewTab),
            pane_select_default_action(PaneSelectMode::MoveToNewWindow),
            RotatePanes(RotationDirection::Clockwise),
            RotatePanes(RotationDirection::CounterClockwise),
            UnifyWindowsOnActiveDomain,
            UnifyAllWindows,
            // --- Swap Layouts & Floating Panes ---
            SwapLayoutNext,
            SwapLayoutPrev,
            ToggleFloatingPane,
            FloatingPaneCommand(FloatingPaneKeyCommand::SnapLeft),
            FloatingPaneCommand(FloatingPaneKeyCommand::SnapRight),
            FloatingPaneCommand(FloatingPaneKeyCommand::RaiseToTop),
            FloatingPaneCommand(FloatingPaneKeyCommand::CycleOverlapping),
            CycleStackForward,
            CycleStackBackward,
            // --- Agent swarm mass operations ---
            KillStuckAgents,
            PauseAllAgents,
            FocusErrorPanes,
            CycleAgentAutoLayout,
            ToggleDashboard,
            ActivateTab(0),
            ActivateTab(1),
            ActivateTab(2),
            ActivateTab(3),
            ActivateTab(4),
            ActivateTab(5),
            ActivateTab(6),
            ActivateTab(7),
            ActivateTab(-1),
            ActivateTabRelative(-1),
            ActivateTabRelative(1),
            ActivateWindow(0),
            ActivateWindow(1),
            ActivateWindow(2),
            ActivateWindow(3),
            ActivateWindow(4),
            ActivateWindow(5),
            ActivateWindow(6),
            ActivateWindow(7),
            ActivateWindow(8),
            ActivateWindow(9),
            ActivateWindowRelative(-1),
            ActivateWindowRelative(1),
            MoveTabRelative(-1),
            MoveTabRelative(1),
            AdjustPaneSize(PaneDirection::Left, 1),
            AdjustPaneSize(PaneDirection::Right, 1),
            AdjustPaneSize(PaneDirection::Up, 1),
            AdjustPaneSize(PaneDirection::Down, 1),
            ActivatePaneDirection(PaneDirection::Left),
            ActivatePaneDirection(PaneDirection::Right),
            ActivatePaneDirection(PaneDirection::Up),
            ActivatePaneDirection(PaneDirection::Down),
            TogglePaneZoomState,
            ActivateLastTab,
            ShowLauncher,
            ShowTabNavigator,
            // ----------------- Help
            OpenUri("https://github.com/Dicklesworthstone/frankenterm".to_string()),
            OpenUri("https://github.com/Dicklesworthstone/frankenterm/discussions/".to_string()),
            OpenUri("https://github.com/Dicklesworthstone/frankenterm/issues/".to_string()),
            ShowDebugOverlay,
            // ----------------- Misc
            OpenLinkAtMouseCursor,
        ]
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn detach_domain_commands_require_detachable_attached_non_local_domain() {
            assert!(domain_detach_command_is_available(
                "remote",
                DomainState::Attached,
                true,
            ));

            assert!(!domain_detach_command_is_available(
                "remote",
                DomainState::Attached,
                false,
            ));
            assert!(!domain_detach_command_is_available(
                "remote",
                DomainState::Detached,
                true,
            ));
            assert!(!domain_detach_command_is_available(
                "local",
                DomainState::Attached,
                true,
            ));
        }

        /// Every `PaneSelect` mode must ship with at least one default chord.
        /// Pre-fix the five rows had `keys: vec![]` and a "FIXME" comment, so a
        /// freshly-installed user could only reach pane-management through the
        /// menu / lua. This test fences that regression.
        #[test]
        fn pane_select_modes_all_carry_default_keybindings() {
            for mode in PANE_SELECT_DEFAULT_MODES {
                let keys = pane_select_default_keys(mode);
                assert!(
                    !keys.is_empty(),
                    "PaneSelectMode::{mode:?} ships without a default chord"
                );
            }
        }

        /// The five `PaneSelect` defaults must all be distinct chords. A
        /// silent collision would mean two modes fire on the same key press,
        /// which is its own accessibility bug.
        #[test]
        fn pane_select_default_chords_are_pairwise_distinct() {
            let chords: Vec<_> = PANE_SELECT_DEFAULT_MODES
                .into_iter()
                .map(|mode| pane_select_default_keys(mode)[0].clone())
                .collect();
            let mut seen = std::collections::HashSet::new();
            for (mods, key) in &chords {
                let label = format!("{mods:?}+{key}");
                assert!(
                    seen.insert(label.clone()),
                    "duplicate PaneSelect default chord: {label}"
                );
            }
        }

        #[test]
        fn unqualified_current_domain_detach_is_not_a_default_palette_action() {
            assert!(
                !compute_default_actions().iter().any(|action| matches!(
                    action,
                    DetachDomain(SpawnTabDomain::CurrentPaneDomain)
                )),
                "CurrentPaneDomain detach depends on the active pane domain being detachable; \
                 generated domain-specific detach entries carry that runtime capability check"
            );
        }
    }
}
pub mod status_text {
    use finl_unicode::grapheme_clusters::Graphemes;
    use termwiz::cell::{Cell, CellAttributes};
    use termwiz::color::ColorSpec;
    use termwiz::escape::csi::Sgr;
    use termwiz::escape::parser::Parser;
    use termwiz::escape::{Action, CSI, ControlCode};
    use termwiz::surface::SEQ_ZERO;
    use wezterm_term::Line;

    const MAX_STATUS_PARSE_CELLS: usize = 4096;
    const MAX_STATUS_PARSE_BYTES: usize = 64 * 1024;
    const STATUS_BYTES_PER_CELL_BUDGET: usize = 64;

    pub fn parse_status_text(text: &str, default_cell: CellAttributes) -> Line {
        parse_status_text_with_cell_limit(text, default_cell, MAX_STATUS_PARSE_CELLS)
    }

    pub fn parse_status_text_with_cell_limit(
        text: &str,
        default_cell: CellAttributes,
        max_cells: usize,
    ) -> Line {
        let max_cells = max_cells.min(MAX_STATUS_PARSE_CELLS);
        if max_cells == 0 {
            return Line::with_width(0, SEQ_ZERO);
        }

        let max_bytes = max_cells
            .saturating_mul(STATUS_BYTES_PER_CELL_BUDGET)
            .clamp(1, MAX_STATUS_PARSE_BYTES);
        let text = status_text_prefix(text, max_bytes);
        let mut pen = default_cell.clone();
        let mut cells = vec![];
        let mut ignoring = false;
        let mut print_buffer = String::new();

        fn flush_print(
            buf: &mut String,
            cells: &mut Vec<Cell>,
            pen: &CellAttributes,
            max_cells: usize,
        ) {
            for g in Graphemes::new(buf.as_str()) {
                if cells.len() >= max_cells {
                    break;
                }
                let cell = Cell::new_grapheme(g, pen.clone(), None);
                let width = cell.width();
                if cells.len().saturating_add(width) > max_cells {
                    break;
                }
                cells.push(cell);
                for _ in 1..width {
                    // Line/Screen expect double wide graphemes to be followed by a blank in
                    // the next column position, otherwise we'll render incorrectly.
                    cells.push(Cell::blank_with_attrs(pen.clone()));
                }
            }
            buf.clear();
        }

        let mut parser = Parser::new();
        parser.parse(text.as_bytes(), |action| {
            if ignoring || cells.len() >= max_cells {
                return;
            }
            match action {
                Action::Print(c) => print_buffer.push(c),
                Action::PrintString(s) => print_buffer.push_str(&s),
                Action::Control(c) => {
                    flush_print(&mut print_buffer, &mut cells, &pen, max_cells);
                    match c {
                        ControlCode::CarriageReturn | ControlCode::LineFeed => {
                            ignoring = true;
                        }
                        _ => {}
                    }
                }
                Action::CSI(csi) => {
                    flush_print(&mut print_buffer, &mut cells, &pen, max_cells);
                    match csi {
                        CSI::Sgr(sgr) => match sgr {
                            Sgr::Reset => pen = default_cell.clone(),
                            Sgr::Intensity(i) => {
                                pen.set_intensity(i);
                            }
                            Sgr::Underline(u) => {
                                pen.set_underline(u);
                            }
                            Sgr::Overline(o) => {
                                pen.set_overline(o);
                            }
                            Sgr::VerticalAlign(o) => {
                                pen.set_vertical_align(o);
                            }
                            Sgr::Blink(b) => {
                                pen.set_blink(b);
                            }
                            Sgr::Italic(i) => {
                                pen.set_italic(i);
                            }
                            Sgr::Inverse(inverse) => {
                                pen.set_reverse(inverse);
                            }
                            Sgr::Invisible(invis) => {
                                pen.set_invisible(invis);
                            }
                            Sgr::StrikeThrough(strike) => {
                                pen.set_strikethrough(strike);
                            }
                            Sgr::Foreground(col) => {
                                if let ColorSpec::Default = col {
                                    pen.set_foreground(default_cell.foreground());
                                } else {
                                    pen.set_foreground(col);
                                }
                            }
                            Sgr::Background(col) => {
                                if let ColorSpec::Default = col {
                                    pen.set_background(default_cell.background());
                                } else {
                                    pen.set_background(col);
                                }
                            }
                            Sgr::UnderlineColor(col) => {
                                pen.set_underline_color(col);
                            }
                            Sgr::Font(_) => {}
                        },
                        _ => {}
                    }
                }
                Action::OperatingSystemCommand(_)
                | Action::DeviceControl(_)
                | Action::Esc(_)
                | Action::KittyImage(_)
                | Action::XtGetTcap(_)
                | Action::Sixel(_) => {
                    flush_print(&mut print_buffer, &mut cells, &pen, max_cells);
                }
            }
        });
        flush_print(&mut print_buffer, &mut cells, &pen, max_cells);
        Line::from_cells(cells, SEQ_ZERO)
    }

    fn status_text_prefix(text: &str, max_bytes: usize) -> &str {
        if text.len() <= max_bytes {
            return text;
        }

        let mut end = max_bytes;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        &text[..end]
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn status_text_parser_respects_cell_limit() {
            let line = parse_status_text_with_cell_limit("abcdef", CellAttributes::default(), 3);

            assert_eq!(line.len(), 3);
            assert_eq!(line.as_str().as_ref(), "abc");
        }

        #[test]
        fn status_text_parser_does_not_split_double_width_graphemes() {
            let line =
                parse_status_text_with_cell_limit("\u{1f600}abc", CellAttributes::default(), 1);

            assert_eq!(line.len(), 0);
        }
    }
}
pub mod selector_math {
    pub fn selector_label_count(filtered_entries_len: usize, max_items: usize) -> usize {
        filtered_entries_len.min(max_items.saturating_add(1))
    }

    pub fn visible_mouse_entry_index(
        top_row: usize,
        y: u16,
        filtered_entries_len: usize,
    ) -> Option<usize> {
        let row_offset = usize::from(y).checked_sub(1)?;
        let active_idx = top_row.saturating_add(row_offset);
        (active_idx < filtered_entries_len).then_some(active_idx)
    }

    #[cfg(test)]
    mod tests {
        use super::{selector_label_count, visible_mouse_entry_index};

        #[test]
        fn selector_label_count_tracks_filtered_entries_with_visible_row() {
            assert_eq!(selector_label_count(2, 10), 2);
            assert_eq!(selector_label_count(9, 10), 9);
        }

        #[test]
        fn selector_label_count_caps_to_visible_rows_plus_one() {
            assert_eq!(selector_label_count(25, 3), 4);
            assert_eq!(selector_label_count(25, 0), 1);
        }

        #[test]
        fn selector_label_count_saturates_extreme_row_capacity() {
            assert_eq!(selector_label_count(usize::MAX, usize::MAX), usize::MAX);
        }

        #[test]
        fn selector_visible_mouse_entry_index_maps_screen_row_to_filtered_entry() {
            assert_eq!(visible_mouse_entry_index(0, 1, 3), Some(0));
            assert_eq!(visible_mouse_entry_index(5, 2, 10), Some(6));
        }

        #[test]
        fn selector_visible_mouse_entry_index_rejects_header_and_filtered_tail() {
            assert_eq!(visible_mouse_entry_index(0, 0, 3), None);
            assert_eq!(visible_mouse_entry_index(8, 4, 10), None);
        }

        #[test]
        fn selector_visible_mouse_entry_index_saturates_scrolled_extremes() {
            assert_eq!(visible_mouse_entry_index(usize::MAX, 2, usize::MAX), None);
        }
    }
}
pub mod smart_selection_a11y;
pub mod status_bar;
pub mod triple_buffer_gui;

pub mod gui_debug_log {
    use chrono::{DateTime, Local};
    use log::Level;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Mutex, MutexGuard};

    const CAPACITY: usize = 256;

    #[derive(Debug, Clone)]
    pub struct GuiDebugLogEntry {
        pub sequence: u64,
        pub then: DateTime<Local>,
        pub level: Level,
        pub target: String,
        pub message: String,
    }

    static NEXT_SEQUENCE: AtomicU64 = AtomicU64::new(1);

    lazy_static::lazy_static! {
        static ref ENTRIES: Mutex<VecDeque<GuiDebugLogEntry>> =
            Mutex::new(VecDeque::with_capacity(CAPACITY));
    }

    fn lock_entries(_context: &str) -> MutexGuard<'static, VecDeque<GuiDebugLogEntry>> {
        ENTRIES.lock().unwrap_or_else(|poisoned| {
            // Avoid log::warn! here: this is the log sink and may re-enter ENTRIES.
            ENTRIES.clear_poison();
            poisoned.into_inner()
        })
    }

    pub fn record(level: Level, target: impl Into<String>, message: impl Into<String>) -> u64 {
        let sequence = NEXT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let entry = GuiDebugLogEntry {
            sequence,
            then: Local::now(),
            level,
            target: target.into(),
            message: message.into(),
        };

        let mut entries = lock_entries("recording log entry");
        if entries.len() == CAPACITY {
            entries.pop_front();
        }
        entries.push_back(entry);
        sequence
    }

    pub fn entries_after(sequence: Option<u64>) -> Vec<GuiDebugLogEntry> {
        let min_sequence = sequence.unwrap_or(0);
        lock_entries("reading log entries")
            .iter()
            .filter(|entry| entry.sequence > min_sequence)
            .cloned()
            .collect()
    }

    #[cfg(test)]
    fn reset_for_tests() {
        NEXT_SEQUENCE.store(1, Ordering::Relaxed);
        lock_entries("resetting test state").clear();
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        lazy_static::lazy_static! {
            static ref TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        }

        fn lock_test() -> std::sync::MutexGuard<'static, ()> {
            TEST_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        }

        #[test]
        fn entries_after_filters_by_sequence() {
            let _guard = lock_test();
            reset_for_tests();

            let first = record(Level::Info, "test", "first");
            let second = record(Level::Warn, "test", "second");

            let entries = entries_after(Some(first));
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].sequence, second);
            assert_eq!(entries[0].message, "second");
        }

        #[test]
        fn entries_are_bounded_to_recent_capacity() {
            let _guard = lock_test();
            reset_for_tests();

            for index in 0..(CAPACITY + 4) {
                record(Level::Info, "test", format!("entry-{index}"));
            }

            let entries = entries_after(None);
            assert_eq!(entries.len(), CAPACITY);
            assert_eq!(entries[0].message, "entry-4");
            assert_eq!(
                entries[CAPACITY - 1].message,
                format!("entry-{}", CAPACITY + 3)
            );
        }

        #[test]
        fn entries_recover_after_poisoned_lock() {
            let _guard = lock_test();
            reset_for_tests();

            let poison_result = std::panic::catch_unwind(|| {
                let _guard = ENTRIES
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                std::panic::resume_unwind(Box::new("simulate GUI debug log poison"));
            });

            assert!(poison_result.is_err());

            let sequence = record(Level::Error, "test", "after poison");
            let entries = entries_after(None);
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].sequence, sequence);
            assert_eq!(entries[0].message, "after poison");
        }
    }
}

#[cfg(any(feature = "debug-cell-crc", test))]
pub mod cell_crc;

#[cfg(feature = "headless-render")]
pub mod headless_render;

#[cfg(test)]
extern crate self as frankenterm_gui;

#[cfg(test)]
#[path = "../tests/ssim_parity.rs"]
mod ssim_parity;
