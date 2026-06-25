use crate::customglyph::BlockKey;
use crate::glyphcache::CachedGlyph;
use ahash::{AHashMap, AHasher};
use config::TextStyle;
use frankenterm_font::shaper::GlyphInfo;
use frankenterm_font::units::*;
use std::cell::RefCell;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

const SHAPED_RUN_INTERNER_MAX_ENTRIES: usize = 8192;
const SHAPED_RUN_INTERNER_MAX_COLLISIONS: usize = 4;

thread_local! {
    static SHAPED_RUN_INTERNER: RefCell<ShapedRunInterner> =
        RefCell::new(ShapedRunInterner::default());
}

#[derive(PartialEq, Eq, Hash, Clone, Debug)]
pub struct ShapeCacheKey {
    pub style: TextStyle,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GlyphPosition {
    pub glyph_idx: u32,
    pub num_cells: u8,
    pub x_offset: PixelLength,
    pub bearing_x: f32,
    pub bitmap_pixel_width: u32,
}

#[derive(Clone, Debug)]
pub struct ShapedInfo {
    pub glyph: Rc<CachedGlyph>,
    pub pos: GlyphPosition,
    pub block_key: Option<BlockKey>,
}

impl ShapedInfo {
    /// Process the results from the shaper, stitching together glyph
    /// and positioning information
    pub fn process(infos: &[GlyphInfo], glyphs: &[Rc<CachedGlyph>]) -> Vec<ShapedInfo> {
        if !glyph_run_interning_enabled() || infos.len() != glyphs.len() || infos.len() < 2 {
            return build_shaped_infos(infos, glyphs);
        }

        let key = shaped_run_key(infos, glyphs);
        if let Some(run) =
            SHAPED_RUN_INTERNER.with(|interner| interner.borrow().lookup(key, infos, glyphs))
        {
            return run;
        }

        let run = build_shaped_infos(infos, glyphs);
        SHAPED_RUN_INTERNER.with(|interner| interner.borrow_mut().insert(key, infos, glyphs, &run));
        run
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ShapedRunKey {
    hash: u64,
    len: usize,
}

#[derive(Default)]
struct ShapedRunInterner {
    runs: AHashMap<ShapedRunKey, Vec<InternedShapedRun>>,
    entries: usize,
}

impl ShapedRunInterner {
    fn lookup(
        &self,
        key: ShapedRunKey,
        infos: &[GlyphInfo],
        glyphs: &[Rc<CachedGlyph>],
    ) -> Option<Vec<ShapedInfo>> {
        self.runs.get(&key).and_then(|runs| {
            runs.iter()
                .find(|run| shaped_run_matches(infos, glyphs, run))
                // Re-attach the CURRENT glyphs (current atlas generation), not
                // the ones captured at intern time — see `rebuild`. This is what
                // makes the interner leak-free: it never holds a `Rc<CachedGlyph>`
                // and so can never keep an old atlas texture alive.
                .map(|run| run.rebuild(glyphs))
        })
    }

    fn insert(
        &mut self,
        key: ShapedRunKey,
        infos: &[GlyphInfo],
        glyphs: &[Rc<CachedGlyph>],
        run: &[ShapedInfo],
    ) {
        if self.entries >= SHAPED_RUN_INTERNER_MAX_ENTRIES {
            self.runs.clear();
            self.entries = 0;
        }

        let bucket = self.runs.entry(key).or_default();
        if bucket.len() >= SHAPED_RUN_INTERNER_MAX_COLLISIONS {
            bucket.swap_remove(0);
            self.entries = self.entries.saturating_sub(1);
        }

        bucket.push(InternedShapedRun::new(infos, glyphs, run));
        self.entries += 1;
    }
}

/// The atlas-generation-INVARIANT slice of a `ShapedInfo`: glyph position and
/// block key. Deliberately excludes `glyph: Rc<CachedGlyph>` (the only
/// atlas-dependent, GPU-resource-owning field) so the interner cannot pin a
/// glyph — and therefore cannot pin the ~tens-of-MB atlas texture that glyph's
/// `Sprite` references. The `GlyphPosition`/`BlockKey` are pure shaping output
/// (from the shaper's `GlyphInfo` + invariant font metrics) and are identical
/// across atlas recreations, so caching them is sound and survives recreation.
#[derive(Clone, Debug)]
struct ShapedInfoTemplate {
    pos: GlyphPosition,
    block_key: Option<BlockKey>,
}

#[derive(Clone, Debug)]
struct InternedShapedRun {
    signature: Vec<ShapedRunGlyph>,
    /// Invariant per-glyph templates. NO `Rc<CachedGlyph>` is stored here — that
    /// is the whole point (ft glyph-run-interner atlas-pinning leak fix): the
    /// shaping cache caches the CPU shaping result, never the GPU residency.
    templates: Vec<ShapedInfoTemplate>,
}

impl InternedShapedRun {
    fn new(infos: &[GlyphInfo], glyphs: &[Rc<CachedGlyph>], shaped: &[ShapedInfo]) -> Self {
        Self {
            signature: shaped_run_signature(infos, glyphs),
            templates: shaped
                .iter()
                .map(|s| ShapedInfoTemplate {
                    pos: s.pos.clone(),
                    block_key: s.block_key,
                })
                .collect(),
        }
    }

    /// Materialize a `Vec<ShapedInfo>` for THIS call by zipping the cached
    /// invariant templates with the caller's CURRENT glyphs (current atlas).
    /// The matching `signature`/`len` guarantee `glyphs` lines up 1:1 with the
    /// templates, so each template gets the live glyph at its index.
    fn rebuild(&self, glyphs: &[Rc<CachedGlyph>]) -> Vec<ShapedInfo> {
        self.templates
            .iter()
            .zip(glyphs.iter())
            .map(|(template, glyph)| ShapedInfo {
                pos: template.pos.clone(),
                glyph: Rc::clone(glyph),
                block_key: template.block_key,
            })
            .collect()
    }

    fn matches(&self, infos: &[GlyphInfo], glyphs: &[Rc<CachedGlyph>]) -> bool {
        self.signature.len() == infos.len()
            && glyphs.len() == infos.len()
            && self
                .signature
                .iter()
                .zip(infos.iter())
                .zip(glyphs.iter())
                .all(|((cached, info), glyph)| *cached == ShapedRunGlyph::new(info, glyph))
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ShapedRunGlyph {
    only_char: Option<char>,
    is_space: bool,
    num_cells: u8,
    cluster: u32,
    font_idx: usize,
    glyph_pos: u32,
    x_advance_bits: u64,
    y_advance_bits: u64,
    x_offset_bits: u64,
    y_offset_bits: u64,
    glyph_ptr: usize,
    bitmap_pixel_width: u32,
    bearing_x_bits: u64,
    block_key: Option<BlockKey>,
}

impl ShapedRunGlyph {
    fn new(info: &GlyphInfo, glyph: &Rc<CachedGlyph>) -> Self {
        Self {
            only_char: info.only_char,
            is_space: info.is_space,
            num_cells: info.num_cells,
            cluster: info.cluster,
            font_idx: info.font_idx,
            glyph_pos: info.glyph_pos,
            x_advance_bits: info.x_advance.get().to_bits(),
            y_advance_bits: info.y_advance.get().to_bits(),
            x_offset_bits: info.x_offset.get().to_bits(),
            y_offset_bits: info.y_offset.get().to_bits(),
            glyph_ptr: Rc::as_ptr(glyph) as usize,
            bitmap_pixel_width: glyph_bitmap_pixel_width(glyph),
            bearing_x_bits: glyph.bearing_x.get().to_bits(),
            block_key: info.only_char.and_then(BlockKey::from_char),
        }
    }
}

#[inline]
fn glyph_run_interning_enabled() -> bool {
    frankenterm_gui::glyph_run_interning::glyph_run_interning_enabled()
}

fn build_shaped_infos(infos: &[GlyphInfo], glyphs: &[Rc<CachedGlyph>]) -> Vec<ShapedInfo> {
    let mut pos: Vec<ShapedInfo> = Vec::with_capacity(infos.len());

    for (info, glyph) in infos.iter().zip(glyphs.iter()) {
        pos.push(ShapedInfo {
            pos: GlyphPosition {
                glyph_idx: info.glyph_pos,
                bitmap_pixel_width: glyph_bitmap_pixel_width(glyph),
                num_cells: info.num_cells,
                x_offset: info.x_offset,
                bearing_x: glyph.bearing_x.get() as f32,
            },
            glyph: Rc::clone(glyph),
            block_key: info.only_char.and_then(BlockKey::from_char),
        });
    }
    pos
}

fn shaped_run_key(infos: &[GlyphInfo], glyphs: &[Rc<CachedGlyph>]) -> ShapedRunKey {
    let mut hasher = AHasher::default();
    infos.len().hash(&mut hasher);

    for (info, glyph) in infos.iter().zip(glyphs.iter()) {
        ShapedRunGlyph::new(info, glyph).hash(&mut hasher);
    }

    ShapedRunKey {
        hash: hasher.finish(),
        len: infos.len(),
    }
}

fn shaped_run_matches(
    infos: &[GlyphInfo],
    glyphs: &[Rc<CachedGlyph>],
    run: &InternedShapedRun,
) -> bool {
    run.matches(infos, glyphs)
}

fn shaped_run_signature(infos: &[GlyphInfo], glyphs: &[Rc<CachedGlyph>]) -> Vec<ShapedRunGlyph> {
    infos
        .iter()
        .zip(glyphs.iter())
        .map(|(info, glyph)| ShapedRunGlyph::new(info, glyph))
        .collect()
}

#[inline]
fn glyph_bitmap_pixel_width(glyph: &CachedGlyph) -> u32 {
    glyph
        .texture
        .as_ref()
        .map_or(0, |t| t.coords.width() as u32)
}

/// We'd like to avoid allocating when resolving from the cache
/// so this is the borrowed version of ShapeCacheKey.
/// It's a bit involved to make this work; more details can be
/// found in the excellent guide here:
/// <https://github.com/sunshowers-code/borrow-complex-key-example/blob/main/src/lib.rs>
#[derive(Copy, Debug, Clone, PartialEq, Eq, Hash)]
pub struct BorrowedShapeCacheKey<'a> {
    pub style: &'a TextStyle,
    pub text: &'a str,
}

impl<'a> BorrowedShapeCacheKey<'a> {
    pub fn to_owned(&self) -> ShapeCacheKey {
        ShapeCacheKey {
            style: self.style.clone(),
            text: self.text.to_owned(),
        }
    }
}

pub trait ShapeCacheKeyTrait: std::fmt::Debug {
    fn key<'k>(&'k self) -> BorrowedShapeCacheKey<'k>;
}

impl ShapeCacheKeyTrait for ShapeCacheKey {
    fn key<'k>(&'k self) -> BorrowedShapeCacheKey<'k> {
        BorrowedShapeCacheKey {
            style: &self.style,
            text: &self.text,
        }
    }
}

impl<'a> ShapeCacheKeyTrait for BorrowedShapeCacheKey<'a> {
    fn key<'k>(&'k self) -> BorrowedShapeCacheKey<'k> {
        *self
    }
}

impl<'a> std::borrow::Borrow<dyn ShapeCacheKeyTrait + 'a> for ShapeCacheKey {
    fn borrow(&self) -> &(dyn ShapeCacheKeyTrait + 'a) {
        self
    }
}

impl<'a> PartialEq for dyn ShapeCacheKeyTrait + 'a {
    fn eq(&self, other: &Self) -> bool {
        self.key().eq(&other.key())
    }
}

impl<'a> Eq for dyn ShapeCacheKeyTrait + 'a {}

impl<'a> std::hash::Hash for dyn ShapeCacheKeyTrait + 'a {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.key().hash(state)
    }
}

#[cfg(test)]
mod test {
    use super::{
        InternedShapedRun, SHAPED_RUN_INTERNER, SHAPED_RUN_INTERNER_MAX_COLLISIONS,
        SHAPED_RUN_INTERNER_MAX_ENTRIES, ShapedRunInterner, ShapedRunKey, build_shaped_infos,
        glyph_run_interning_enabled, shaped_run_key, shaped_run_signature,
    };
    use crate::glyphcache::{CachedGlyph, GlyphCache};
    use crate::shapecache::{GlyphPosition, ShapedInfo};
    use crate::utilsprites::RenderMetrics;
    use config::{FontAttributes, TextStyle};
    use euclid::num::Zero;
    use frankenterm_font::shaper::{GlyphInfo, PresentationWidth};
    use frankenterm_font::units::PixelLength;
    use frankenterm_font::{FontConfiguration, LoadedFont};
    use std::rc::Rc;
    use termwiz::cell::CellAttributes;
    use termwiz::surface::{Line, SEQ_ZERO};
    use wezterm_bidi::Direction;

    fn shape_infos_and_glyphs(
        render_metrics: &RenderMetrics,
        glyph_cache: &mut GlyphCache,
        style: &TextStyle,
        font: &Rc<LoadedFont>,
        text: &str,
    ) -> (Vec<GlyphInfo>, Vec<Rc<CachedGlyph>>) {
        let line = Line::from_text(text, &CellAttributes::default(), SEQ_ZERO, None);
        let mut all_infos = vec![];
        let mut all_glyphs = vec![];

        for cluster in line.cluster(None) {
            let presentation_width = PresentationWidth::with_cluster(&cluster);
            let mut infos = font
                .shape(
                    &cluster.text,
                    || {},
                    |_| {},
                    None,
                    Direction::LeftToRight,
                    None,
                    Some(&presentation_width),
                )
                .unwrap();
            let mut glyphs = infos
                .iter()
                .map(|info| {
                    let cell_idx = cluster.byte_to_cell_idx(info.cluster as usize);
                    let num_cells = cluster.byte_to_cell_width(info.cluster as usize);

                    let followed_by_space = match line.get_cell(cell_idx + 1) {
                        Some(cell) => cell.str() == " ",
                        None => false,
                    };

                    glyph_cache
                        .cached_glyph(
                            info,
                            style,
                            followed_by_space,
                            font,
                            render_metrics,
                            num_cells,
                        )
                        .unwrap()
                })
                .collect::<Vec<_>>();

            all_infos.append(&mut infos);
            all_glyphs.append(&mut glyphs);
        }

        (all_infos, all_glyphs)
    }

    fn fake_glyph_info(seed: u32, cluster: u32, only_char: char) -> GlyphInfo {
        GlyphInfo {
            text: only_char.to_string(),
            only_char: Some(only_char),
            is_space: only_char == ' ',
            num_cells: 1,
            cluster,
            font_idx: 0,
            glyph_pos: 10_000 + seed,
            x_advance: PixelLength::new(seed as f64 + 1.0),
            y_advance: PixelLength::zero(),
            x_offset: PixelLength::new(seed as f64 / 16.0),
            y_offset: PixelLength::zero(),
        }
    }

    fn fake_cached_glyph(seed: u32) -> Rc<CachedGlyph> {
        Rc::new(CachedGlyph {
            has_color: false,
            brightness_adjust: 1.0,
            x_offset: PixelLength::new(seed as f64 / 32.0),
            y_offset: PixelLength::zero(),
            x_advance: PixelLength::new(seed as f64 + 2.0),
            bearing_x: PixelLength::new(seed as f64 / 8.0),
            bearing_y: PixelLength::zero(),
            texture: None,
            scale: 1.0,
        })
    }

    fn fake_shaped_run(seed: u32) -> (Vec<GlyphInfo>, Vec<Rc<CachedGlyph>>, Vec<ShapedInfo>) {
        let infos = vec![
            fake_glyph_info(seed * 2, 0, 'a'),
            fake_glyph_info(seed * 2 + 1, 1, 'b'),
        ];
        let glyphs = vec![fake_cached_glyph(seed * 2), fake_cached_glyph(seed * 2 + 1)];
        let shaped = build_shaped_infos(&infos, &glyphs);
        (infos, glyphs, shaped)
    }

    fn cluster_and_shape(
        render_metrics: &RenderMetrics,
        glyph_cache: &mut GlyphCache,
        style: &TextStyle,
        font: &Rc<LoadedFont>,
        text: &str,
    ) -> Vec<GlyphPosition> {
        let (all_infos, all_glyphs) =
            shape_infos_and_glyphs(render_metrics, glyph_cache, style, font, text);

        eprintln!("infos: {:#?}", all_infos);
        eprintln!("glyphs: {:#?}", all_glyphs);
        ShapedInfo::process(&all_infos, &all_glyphs)
            .into_iter()
            .map(|p| p.pos)
            .collect()
    }

    fn assert_shaping_stream_eq(
        left_infos: &[GlyphInfo],
        left_glyphs: &[Rc<CachedGlyph>],
        right_infos: &[GlyphInfo],
        right_glyphs: &[Rc<CachedGlyph>],
    ) {
        assert_eq!(
            shaped_run_signature(left_infos, left_glyphs),
            shaped_run_signature(right_infos, right_glyphs)
        );
    }

    fn assert_shaped_runs_identical(left: &[ShapedInfo], right: &[ShapedInfo]) {
        assert_eq!(left.len(), right.len());

        for (idx, (left, right)) in left.iter().zip(right.iter()).enumerate() {
            assert_eq!(left.pos, right.pos, "glyph position differed at {idx}");
            assert_eq!(
                left.block_key, right.block_key,
                "block glyph key differed at {idx}"
            );
            assert!(
                Rc::ptr_eq(&left.glyph, &right.glyph),
                "cached glyph identity differed at {idx}"
            );
        }
    }

    fn assert_signature_mismatch_rejects_hit(
        interned: &InternedShapedRun,
        infos: &[GlyphInfo],
        glyphs: &[Rc<CachedGlyph>],
    ) {
        let mut wrong_glyph_id = interned.clone();
        wrong_glyph_id.signature[0].glyph_pos ^= 1;
        assert!(
            !wrong_glyph_id.matches(infos, glyphs),
            "glyph-id mismatch must not be accepted as an interner hit"
        );

        let mut wrong_advance = interned.clone();
        wrong_advance.signature[0].x_advance_bits ^= 1;
        assert!(
            !wrong_advance.matches(infos, glyphs),
            "x-advance mismatch must not be accepted as an interner hit"
        );

        let mut wrong_cluster = interned.clone();
        wrong_cluster.signature[0].cluster = wrong_cluster.signature[0].cluster.wrapping_add(1);
        assert!(
            !wrong_cluster.matches(infos, glyphs),
            "cluster mismatch must not be accepted as an interner hit"
        );
    }

    #[test]
    fn interner_rejects_forced_key_collision_and_stale_cluster_mapping() {
        let collision_key = ShapedRunKey {
            hash: 0xfeed_cafe_dead_beef,
            len: 2,
        };
        let mut interner = ShapedRunInterner::default();
        let (first_infos, first_glyphs, first_run) = fake_shaped_run(1);
        let (second_infos, second_glyphs, second_run) = fake_shaped_run(2);

        interner.insert(collision_key, &first_infos, &first_glyphs, &first_run);
        assert!(
            interner
                .lookup(collision_key, &second_infos, &second_glyphs)
                .is_none(),
            "same-key collision must not return a different interned run"
        );

        interner.insert(collision_key, &second_infos, &second_glyphs, &second_run);
        let first_hit = interner
            .lookup(collision_key, &first_infos, &first_glyphs)
            .expect("first colliding run must remain addressable by signature");
        assert_shaped_runs_identical(&first_run, &first_hit);
        let second_hit = interner
            .lookup(collision_key, &second_infos, &second_glyphs)
            .expect("second colliding run must be addressable by signature");
        assert_shaped_runs_identical(&second_run, &second_hit);

        let mut stale_cluster_infos = first_infos.clone();
        stale_cluster_infos[1].cluster = stale_cluster_infos[1].cluster.wrapping_add(17);
        assert!(
            interner
                .lookup(collision_key, &stale_cluster_infos, &first_glyphs)
                .is_none(),
            "stale cluster/byte mapping must not hit an interned run for another text"
        );
    }

    /// Regression guard for the glyph-run-interner atlas-generation GPU leak.
    ///
    /// The interner must cache only the atlas-INVARIANT shaping result
    /// (`GlyphPosition` + `BlockKey`) and hold ZERO strong refs to
    /// `Rc<CachedGlyph>`. A `CachedGlyph` transitively owns a GPU atlas texture
    /// (`CachedGlyph.texture -> Sprite -> Rc<dyn Texture2d>`, ~tens of MB each).
    /// If the interner ever pins a glyph, every atlas recreation leaks the old
    /// atlas generation — the ~691 MB IOSurface footprint observed in the field.
    /// This test fails the moment anyone reintroduces a strong glyph ref.
    #[test]
    fn interner_does_not_pin_glyphs_atlas_generation_leak_guard() {
        let (infos, glyphs, run) = fake_shaped_run(7);
        let key = shaped_run_key(&infos, &glyphs);

        let mut interner = ShapedRunInterner::default();
        interner.insert(key, &infos, &glyphs, &run);
        drop(run); // drop the freshly-built run's own glyph clones

        // After interning, the ONLY strong refs to each glyph must be the local
        // `glyphs` Vec (count == 1). count > 1 ⇒ the interner pinned the glyph ⇒
        // its atlas generation can never be reclaimed ⇒ the leak is back.
        for (idx, glyph) in glyphs.iter().enumerate() {
            assert_eq!(
                Rc::strong_count(glyph),
                1,
                "interner retained a strong ref to glyph {idx}: this reintroduces the \
                 atlas-generation GPU-texture leak (the shaping cache must not own GPU \
                 resources — cache the invariant shaping, re-attach the live glyph on hit)"
            );
        }

        // The cache must still resolve, rebuilding against the CURRENT glyphs —
        // this is exactly what lets it survive atlas recreation WITHOUT leaking.
        let hit = interner
            .lookup(key, &infos, &glyphs)
            .expect("interned run must still resolve by signature");
        assert_eq!(hit.len(), glyphs.len(), "rebuilt run length must match");
        for (idx, (hit_info, glyph)) in hit.iter().zip(glyphs.iter()).enumerate() {
            assert!(
                Rc::ptr_eq(&hit_info.glyph, glyph),
                "lookup must re-attach the CURRENT glyph at index {idx}, not a stale one"
            );
        }
    }

    #[test]
    fn interner_enforces_collision_bucket_and_entry_bounds() {
        let collision_key = ShapedRunKey {
            hash: 0x1234,
            len: 2,
        };
        let mut interner = ShapedRunInterner::default();
        for seed in 0..(SHAPED_RUN_INTERNER_MAX_COLLISIONS as u32 + 2) {
            let (infos, glyphs, run) = fake_shaped_run(100 + seed);
            interner.insert(collision_key, &infos, &glyphs, &run);
        }
        assert_eq!(
            interner.runs.get(&collision_key).map(Vec::len),
            Some(SHAPED_RUN_INTERNER_MAX_COLLISIONS),
            "collision buckets must stay bounded"
        );
        assert_eq!(
            interner.entries, SHAPED_RUN_INTERNER_MAX_COLLISIONS,
            "entry accounting must follow collision eviction"
        );

        let mut interner = ShapedRunInterner::default();
        for seed in 0..=(SHAPED_RUN_INTERNER_MAX_ENTRIES as u32) {
            let key = ShapedRunKey {
                hash: u64::from(seed),
                len: 2,
            };
            let (infos, glyphs, run) = fake_shaped_run(1_000 + seed);
            interner.insert(key, &infos, &glyphs, &run);
        }
        assert!(
            interner.entries <= SHAPED_RUN_INTERNER_MAX_ENTRIES,
            "global interner entries must stay bounded"
        );
        assert!(
            interner.runs.len() <= SHAPED_RUN_INTERNER_MAX_ENTRIES,
            "global interner buckets must stay bounded"
        );
    }

    #[test]
    fn interned_run_matches_fresh_shape_for_same_font_and_attrs() {
        config::use_test_configuration();

        let config = config::configuration();
        let fonts = Rc::new(
            FontConfiguration::new(
                None,
                config.dpi.unwrap_or_else(::window::default_dpi) as usize,
            )
            .unwrap(),
        );
        let render_metrics = RenderMetrics::new(&fonts).unwrap();
        let mut glyph_cache = GlyphCache::new_in_memory(&fonts, 128).unwrap();

        let style = TextStyle::default();
        let font = fonts.resolve_font(&style).unwrap();
        let text = "status != ready <= retry";

        assert!(
            glyph_run_interning_enabled(),
            "glyph-run interning must be enabled for the equivalence gate"
        );
        SHAPED_RUN_INTERNER.with(|interner| {
            *interner.borrow_mut() = Default::default();
        });

        let (miss_infos, miss_glyphs) =
            shape_infos_and_glyphs(&render_metrics, &mut glyph_cache, &style, &font, text);
        let fresh_miss_run = build_shaped_infos(&miss_infos, &miss_glyphs);
        let miss_run = ShapedInfo::process(&miss_infos, &miss_glyphs);
        assert_shaped_runs_identical(&fresh_miss_run, &miss_run);

        let (hit_infos, hit_glyphs) =
            shape_infos_and_glyphs(&render_metrics, &mut glyph_cache, &style, &font, text);
        assert_shaping_stream_eq(&miss_infos, &miss_glyphs, &hit_infos, &hit_glyphs);

        let fresh_hit_run = build_shaped_infos(&hit_infos, &hit_glyphs);
        let hit_key = shaped_run_key(&hit_infos, &hit_glyphs);
        let lookup_hit = SHAPED_RUN_INTERNER
            .with(|interner| interner.borrow().lookup(hit_key, &hit_infos, &hit_glyphs))
            .expect("second identical shape must hit the glyph-run interner");
        assert_shaped_runs_identical(&fresh_hit_run, &lookup_hit);

        let interned_hit = SHAPED_RUN_INTERNER.with(|interner| {
            interner
                .borrow()
                .runs
                .get(&hit_key)
                .and_then(|runs| runs.iter().find(|run| run.matches(&hit_infos, &hit_glyphs)))
                .cloned()
        });
        let interned_hit = interned_hit.expect("matching interned run must be retained");
        assert_eq!(
            shaped_run_signature(&hit_infos, &hit_glyphs),
            interned_hit.signature
        );
        assert_signature_mismatch_rejects_hit(&interned_hit, &hit_infos, &hit_glyphs);

        let process_hit = ShapedInfo::process(&hit_infos, &hit_glyphs);
        assert_shaped_runs_identical(&fresh_hit_run, &process_hit);
    }

    #[test]
    fn ligatures_fira() {
        config::use_test_configuration();
        let _ = env_logger::Builder::new()
            .is_test(true)
            .filter_level(log::LevelFilter::Trace)
            .try_init();

        let config = config::configuration();

        let mut config: config::Config = (*config).clone();
        config.font = TextStyle {
            font: vec![FontAttributes::new("Fira Code")],
            foreground: None,
        };
        config.font_rules.clear();
        config.compute_extra_defaults(None);
        config::use_this_configuration(config.clone());

        let fonts = Rc::new(
            FontConfiguration::new(
                None,
                config.dpi.unwrap_or_else(::window::default_dpi) as usize,
            )
            .unwrap(),
        );
        let render_metrics = RenderMetrics::new(&fonts).unwrap();
        let mut glyph_cache = GlyphCache::new_in_memory(&fonts, 128).unwrap();

        let style = TextStyle::default();
        let font = fonts.resolve_font(&style).unwrap();

        k9::snapshot!(
            cluster_and_shape(&render_metrics, &mut glyph_cache, &style, &font, "a..."),
            "
[
    GlyphPosition {
        glyph_idx: 189,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 1.0,
        bitmap_pixel_width: 8,
    },
    GlyphPosition {
        glyph_idx: 1742,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 0,
    },
    GlyphPosition {
        glyph_idx: 1742,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 0,
    },
    GlyphPosition {
        glyph_idx: 896,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: -15.0,
        bitmap_pixel_width: 20,
    },
]
"
        );
    }

    #[test]
    fn bench_shaping() {
        config::use_test_configuration();

        // let mut glyph_cache = GlyphCache::new_in_memory(&fonts, 128, &render_metrics).unwrap();
        // let render_metrics = RenderMetrics::new(&fonts).unwrap();

        benchmarking::warm_up();

        for &n in &[100, 1000, 10_000] {
            let bench_result = benchmarking::measure_function(move |measurer| {
                let text: String = (0..n).map(|_| ' ').collect();

                let fonts = Rc::new(
                    FontConfiguration::new(
                        None,
                        config::configuration()
                            .dpi
                            .unwrap_or_else(::window::default_dpi) as usize,
                    )
                    .unwrap(),
                );
                let style = TextStyle::default();
                let font = fonts.resolve_font(&style).unwrap();
                let line = Line::from_text(&text, &CellAttributes::default(), SEQ_ZERO, None);
                let cell_clusters = line.cluster(None);
                let cluster = &cell_clusters[0];
                let presentation_width = PresentationWidth::with_cluster(cluster);

                measurer.measure(|| {
                    let _x = font
                        .shape(
                            &cluster.text,
                            || {},
                            |_| {},
                            None,
                            Direction::LeftToRight,
                            None,
                            Some(&presentation_width),
                        )
                        .unwrap();
                    // println!("{:?}", &x[0..2]);
                });
            })
            .unwrap();
            println!("{}: {:?}", n, bench_result.elapsed());
        }
    }

    #[test]
    fn ligatures_jetbrains() {
        config::use_test_configuration();
        let _ = env_logger::Builder::new()
            .is_test(true)
            .filter_level(log::LevelFilter::Trace)
            .try_init();
        let config = config::configuration();

        let fonts = Rc::new(
            FontConfiguration::new(
                None,
                config.dpi.unwrap_or_else(::window::default_dpi) as usize,
            )
            .unwrap(),
        );
        let render_metrics = RenderMetrics::new(&fonts).unwrap();
        let mut glyph_cache = GlyphCache::new_in_memory(&fonts, 128).unwrap();

        let style = TextStyle::default();
        let font = fonts.resolve_font(&style).unwrap();

        k9::snapshot!(
            cluster_and_shape(&render_metrics, &mut glyph_cache, &style, &font, "ab"),
            "
[
    GlyphPosition {
        glyph_idx: 189,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 1.0,
        bitmap_pixel_width: 8,
    },
    GlyphPosition {
        glyph_idx: 214,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 1.0,
        bitmap_pixel_width: 8,
    },
]
"
        );

        k9::snapshot!(
            cluster_and_shape(&render_metrics, &mut glyph_cache, &style, &font, "a b"),
            "
[
    GlyphPosition {
        glyph_idx: 189,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 1.0,
        bitmap_pixel_width: 8,
    },
    GlyphPosition {
        glyph_idx: 958,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 0,
    },
    GlyphPosition {
        glyph_idx: 214,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 1.0,
        bitmap_pixel_width: 8,
    },
]
"
        );

        k9::snapshot!(
            cluster_and_shape(&render_metrics, &mut glyph_cache, &style, &font, "a..."),
            "
[
    GlyphPosition {
        glyph_idx: 189,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 1.0,
        bitmap_pixel_width: 8,
    },
    GlyphPosition {
        glyph_idx: 1742,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 0,
    },
    GlyphPosition {
        glyph_idx: 1742,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 0,
    },
    GlyphPosition {
        glyph_idx: 896,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: -15.0,
        bitmap_pixel_width: 20,
    },
]
"
        );

        k9::snapshot!(
            cluster_and_shape(&render_metrics, &mut glyph_cache, &style, &font, "e_or_"),
            "
[
    GlyphPosition {
        glyph_idx: 225,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 1.0,
        bitmap_pixel_width: 8,
    },
    GlyphPosition {
        glyph_idx: 860,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 9,
    },
    GlyphPosition {
        glyph_idx: 290,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 1.0,
        bitmap_pixel_width: 8,
    },
    GlyphPosition {
        glyph_idx: 320,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 1.0,
        bitmap_pixel_width: 8,
    },
    GlyphPosition {
        glyph_idx: 860,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 9,
    },
]
"
        );

        k9::snapshot!(
            cluster_and_shape(&render_metrics, &mut glyph_cache, &style, &font, "a  b"),
            "
[
    GlyphPosition {
        glyph_idx: 189,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 1.0,
        bitmap_pixel_width: 8,
    },
    GlyphPosition {
        glyph_idx: 958,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 0,
    },
    GlyphPosition {
        glyph_idx: 958,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 0,
    },
    GlyphPosition {
        glyph_idx: 214,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 1.0,
        bitmap_pixel_width: 8,
    },
]
"
        );

        k9::snapshot!(
            cluster_and_shape(&render_metrics, &mut glyph_cache, &style, &font, "<-"),
            "
[
    GlyphPosition {
        glyph_idx: 1742,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 0,
    },
    GlyphPosition {
        glyph_idx: 1588,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: -9.0,
        bitmap_pixel_width: 17,
    },
]
"
        );

        k9::snapshot!(
            cluster_and_shape(&render_metrics, &mut glyph_cache, &style, &font, "<>"),
            "
[
    GlyphPosition {
        glyph_idx: 1742,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 0,
    },
    GlyphPosition {
        glyph_idx: 1613,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: -8.0,
        bitmap_pixel_width: 16,
    },
]
"
        );

        k9::snapshot!(
            cluster_and_shape(&render_metrics, &mut glyph_cache, &style, &font, "|=>"),
            "
[
    GlyphPosition {
        glyph_idx: 1742,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 0,
    },
    GlyphPosition {
        glyph_idx: 1742,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 0,
    },
    GlyphPosition {
        glyph_idx: 1562,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: -18.0,
        bitmap_pixel_width: 27,
    },
]
"
        );

        let block_bottom_one_eighth = "\u{2581}";
        k9::snapshot!(
            cluster_and_shape(
                &render_metrics,
                &mut glyph_cache,
                &style,
                &font,
                block_bottom_one_eighth
            ),
            "
[
    GlyphPosition {
        glyph_idx: 1178,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 10,
    },
]
"
        );

        let powerline_extra_honeycomb = "\u{e0cc}";
        k9::snapshot!(
            cluster_and_shape(
                &render_metrics,
                &mut glyph_cache,
                &style,
                &font,
                powerline_extra_honeycomb,
            ),
            "
[
    GlyphPosition {
        glyph_idx: 58,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: -0.8333333,
        bitmap_pixel_width: 12,
    },
]
"
        );

        k9::snapshot!(
            cluster_and_shape(&render_metrics, &mut glyph_cache, &style, &font, "<!--"),
            "
[
    GlyphPosition {
        glyph_idx: 1742,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 0,
    },
    GlyphPosition {
        glyph_idx: 1742,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 0,
    },
    GlyphPosition {
        glyph_idx: 1742,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 0,
    },
    GlyphPosition {
        glyph_idx: 1595,
        num_cells: 1,
        x_offset: 0.0,
        bearing_x: -28.0,
        bitmap_pixel_width: 37,
    },
]
"
        );

        let deaf_man_medium_light_skin_tone = "\u{1F9CF}\u{1F3FC}\u{200D}\u{2642}\u{FE0F}";
        println!(
            "deaf_man_medium_light_skin_tone: {}",
            deaf_man_medium_light_skin_tone
        );
        k9::snapshot!(
            cluster_and_shape(
                &render_metrics,
                &mut glyph_cache,
                &style,
                &font,
                deaf_man_medium_light_skin_tone
            ),
            "
[
    GlyphPosition {
        glyph_idx: 2712,
        num_cells: 2,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 16,
    },
]
"
        );

        let england_flag = "\u{1F3F4}\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}";
        println!("england_flag: {}", england_flag);
        k9::snapshot!(
            cluster_and_shape(
                &render_metrics,
                &mut glyph_cache,
                &style,
                &font,
                england_flag
            ),
            "
[
    GlyphPosition {
        glyph_idx: 3855,
        num_cells: 2,
        x_offset: 0.0,
        bearing_x: 0.0,
        bitmap_pixel_width: 20,
    },
]
"
        );
    }
}
