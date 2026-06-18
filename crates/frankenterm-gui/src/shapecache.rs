#![allow(unexpected_cfgs)]

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

#[cfg(ft_disable_glyph_run_interning)]
const GLYPH_RUN_INTERNING_CFG_ENABLED: bool = false;

#[cfg(not(ft_disable_glyph_run_interning))]
const GLYPH_RUN_INTERNING_CFG_ENABLED: bool = true;

static GLYPH_RUN_INTERNING_ENV_ENABLED: std::sync::LazyLock<bool> =
    std::sync::LazyLock::new(|| std::env::var_os("FT_DISABLE_GLYPH_RUN_INTERNING").is_none());

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
        SHAPED_RUN_INTERNER.with(|interner| interner.borrow_mut().insert(key, &run));
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
    runs: AHashMap<ShapedRunKey, Vec<Vec<ShapedInfo>>>,
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
                .cloned()
        })
    }

    fn insert(&mut self, key: ShapedRunKey, run: &[ShapedInfo]) {
        if self.entries >= SHAPED_RUN_INTERNER_MAX_ENTRIES {
            self.runs.clear();
            self.entries = 0;
        }

        let bucket = self.runs.entry(key).or_default();
        if bucket.len() >= SHAPED_RUN_INTERNER_MAX_COLLISIONS {
            bucket.swap_remove(0);
            self.entries = self.entries.saturating_sub(1);
        }

        bucket.push(run.to_vec());
        self.entries += 1;
    }
}

#[inline]
fn glyph_run_interning_enabled() -> bool {
    GLYPH_RUN_INTERNING_CFG_ENABLED && *GLYPH_RUN_INTERNING_ENV_ENABLED
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
        info.glyph_pos.hash(&mut hasher);
        info.num_cells.hash(&mut hasher);
        info.x_offset.get().to_bits().hash(&mut hasher);
        (Rc::as_ptr(glyph) as usize).hash(&mut hasher);
        glyph_bitmap_pixel_width(glyph).hash(&mut hasher);
        glyph.bearing_x.get().to_bits().hash(&mut hasher);
        info.only_char
            .and_then(BlockKey::from_char)
            .hash(&mut hasher);
    }

    ShapedRunKey {
        hash: hasher.finish(),
        len: infos.len(),
    }
}

fn shaped_run_matches(
    infos: &[GlyphInfo],
    glyphs: &[Rc<CachedGlyph>],
    run: &[ShapedInfo],
) -> bool {
    run.len() == infos.len()
        && glyphs.len() == infos.len()
        && infos
            .iter()
            .zip(glyphs.iter())
            .zip(run.iter())
            .all(|((info, glyph), shaped)| {
                Rc::ptr_eq(&shaped.glyph, glyph)
                    && shaped.pos.glyph_idx == info.glyph_pos
                    && shaped.pos.num_cells == info.num_cells
                    && shaped.pos.x_offset == info.x_offset
                    && shaped.pos.bearing_x == glyph.bearing_x.get() as f32
                    && shaped.pos.bitmap_pixel_width == glyph_bitmap_pixel_width(glyph)
                    && shaped.block_key == info.only_char.and_then(BlockKey::from_char)
            })
}

#[inline]
fn glyph_bitmap_pixel_width(glyph: &CachedGlyph) -> u32 {
    glyph.texture.as_ref().map_or(0, |t| t.coords.width() as u32)
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
        SHAPED_RUN_INTERNER, build_shaped_infos, glyph_run_interning_enabled, shaped_run_key,
    };
    use crate::glyphcache::{CachedGlyph, GlyphCache};
    use crate::shapecache::{GlyphPosition, ShapedInfo};
    use crate::utilsprites::RenderMetrics;
    use config::{FontAttributes, TextStyle};
    use frankenterm_font::shaper::{GlyphInfo, PresentationWidth};
    use frankenterm_font::{FontConfiguration, LoadedFont};
    use std::rc::Rc;
    use termwiz::cell::CellAttributes;
    use termwiz::surface::{Line, SEQ_ZERO};
    use wezterm_bidi::Direction;

    fn cluster_and_shape(
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

    fn assert_shaping_stream_eq(left: &[GlyphInfo], right: &[GlyphInfo]) {
        let left_stream: Vec<_> = left
            .iter()
            .map(|info| {
                (
                    info.glyph_pos,
                    info.cluster,
                    info.x_advance.get().to_bits(),
                    info.y_advance.get().to_bits(),
                    info.x_offset.get().to_bits(),
                    info.y_offset.get().to_bits(),
                    info.num_cells,
                )
            })
            .collect();
        let right_stream: Vec<_> = right
            .iter()
            .map(|info| {
                (
                    info.glyph_pos,
                    info.cluster,
                    info.x_advance.get().to_bits(),
                    info.y_advance.get().to_bits(),
                    info.x_offset.get().to_bits(),
                    info.y_offset.get().to_bits(),
                    info.num_cells,
                )
            })
            .collect();

        assert_eq!(left_stream, right_stream);
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
        assert_shaping_stream_eq(&miss_infos, &hit_infos);

        let fresh_hit_run = build_shaped_infos(&hit_infos, &hit_glyphs);
        let hit_key = shaped_run_key(&hit_infos, &hit_glyphs);
        let lookup_hit = SHAPED_RUN_INTERNER
            .with(|interner| interner.borrow().lookup(hit_key, &hit_infos, &hit_glyphs))
            .expect("second identical shape must hit the glyph-run interner");
        assert_shaped_runs_identical(&fresh_hit_run, &lookup_hit);

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
