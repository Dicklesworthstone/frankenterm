use crate::parser::ParsedFont;
use crate::shaper::{FallbackIdx, FontMetrics, FontShaper, GlyphInfo, PresentationWidth};
use crate::units::*;
use crate::{ftwrap, hbwrap as harfbuzz};
use anyhow::{anyhow, Context};
use config::ConfigHandle;
use finl_unicode::grapheme_clusters::Graphemes;
use log::error;
use ordered_float::NotNan;
use std::cell::{RefCell, RefMut};
use std::collections::{HashMap, VecDeque};
use std::ops::Range;
use termwiz::cell::{unicode_column_width, Presentation};
use wezterm_bidi::Direction;

// Changing these will switch to using harfbuzz's opentype functions.
// There's something awry with our integration in that mode: the advances
// aren't equivalent to its freetype functions and this manifests most
// prominently in proportional fonts as well as with fonts with bitmap
// strikes.
// Until we get to the bottom of this, these are compile-time rather
// than runtime configs.
const USE_OT_FUNCS: bool = false;
const USE_OT_FACE: bool = false;

// Missing glyphs can visit the same fallback faces for every cluster in a
// frame. Keep a few of those faces warm instead of repeatedly opening them
// and loading/hinting all of their cell-metric probe glyphs. Successful faces
// retain their existing lifetime; this bounds only the additional residency.
const MAX_UNUSED_FALLBACK_FONTS: usize = 4;

fn retain_unused_fallback_fonts() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var_os("FT_DISABLE_UNUSED_FALLBACK_FONT_CACHE").as_deref()
            != Some(std::ffi::OsStr::new("1"))
    })
}

#[derive(Clone, Debug)]
struct Info {
    cluster: usize,
    len: usize,
    codepoint: harfbuzz::hb_codepoint_t,
    x_advance: harfbuzz::hb_position_t,
    y_advance: harfbuzz::hb_position_t,
    x_offset: harfbuzz::hb_position_t,
    y_offset: harfbuzz::hb_position_t,
}

fn get_only_char(s: &str) -> Option<char> {
    let mut chars = s.chars();
    let first_char = chars.next()?;
    if chars.next().is_some() {
        None
    } else {
        Some(first_char)
    }
}

fn make_glyphinfo(text: &str, num_cells: u8, font_idx: usize, info: &Info) -> GlyphInfo {
    let is_space = text == " ";
    let only_char = get_only_char(text);
    GlyphInfo {
        #[cfg(any(debug_assertions, test))]
        text: text.into(),
        only_char,
        is_space,
        num_cells,
        font_idx,
        glyph_pos: info.codepoint,
        cluster: info.cluster as u32,
        x_advance: PixelLength::new(f64::from(info.x_advance) / 64.0),
        y_advance: PixelLength::new(f64::from(info.y_advance) / 64.0),
        x_offset: PixelLength::new(f64::from(info.x_offset) / 64.0),
        y_offset: PixelLength::new(f64::from(info.y_offset) / 64.0),
    }
}

struct FontPair {
    face: ftwrap::Face,
    font: RefCell<harfbuzz::Font>,
    shaped_any: bool,
    presentation: Presentation,
    features: Vec<harfbuzz::hb_feature_t>,
    last_size_and_dpi: RefCell<Option<(f64, u32)>>,
}

#[derive(Debug, Hash, PartialEq, Eq, Clone)]
struct MetricsKey {
    font_idx: usize,
    size: NotNan<f64>,
    dpi: u32,
}

pub struct HarfbuzzShaper {
    handles: Vec<ParsedFont>,
    fonts: Vec<RefCell<Option<FontPair>>>,
    unused_fonts: RefCell<VecDeque<FallbackIdx>>,
    lib: ftwrap::Library,
    metrics: RefCell<HashMap<MetricsKey, FontMetrics>>,
    features: Vec<harfbuzz::hb_feature_t>,
    lang: harfbuzz::hb_language_t,
}

/// Make a string holding a set of unicode replacement
/// characters equal to the number of graphemes in the
/// original string.  That isn't perfect, but it should
/// be good enough to indicate that something isn't right.
fn make_question_string(s: &str) -> String {
    let len = Graphemes::new(s).count();
    let mut result = String::new();
    let c = if !is_question_string(s) {
        std::char::REPLACEMENT_CHARACTER
    } else {
        '?'
    };
    for _ in 0..len {
        result.push(c);
    }
    result
}

fn is_question_string(s: &str) -> bool {
    for c in s.chars() {
        if c != std::char::REPLACEMENT_CHARACTER {
            return false;
        }
    }
    true
}

impl HarfbuzzShaper {
    pub fn new(config: &ConfigHandle, handles: &[ParsedFont]) -> anyhow::Result<Self> {
        let lib = ftwrap::Library::new()?;
        let handles = handles.to_vec();
        let mut fonts = vec![];
        for _ in 0..handles.len() {
            fonts.push(RefCell::new(None));
        }

        let lang = harfbuzz::language_from_string("en")?;

        let features: Vec<harfbuzz::hb_feature_t> = config
            .harfbuzz_features
            .iter()
            .filter_map(|s| harfbuzz::feature_from_string(s).ok())
            .collect();

        Ok(Self {
            fonts,
            unused_fonts: RefCell::new(VecDeque::new()),
            handles,
            lib,
            metrics: RefCell::new(HashMap::new()),
            features,
            lang,
        })
    }

    fn load_fallback(&self, font_idx: FallbackIdx) -> anyhow::Result<Option<RefMut<'_, FontPair>>> {
        if font_idx >= self.handles.len() {
            return Ok(None);
        }
        match self.fonts.get(font_idx) {
            None => Ok(None),
            Some(opt_pair) => {
                let mut opt_pair = opt_pair.borrow_mut();
                if opt_pair.is_none() {
                    let handle = &self.handles[font_idx];
                    log::trace!("shaper wants {} {:?}", font_idx, handle);
                    let face = self.lib.face_from_locator(&handle.handle)?;

                    let font = if USE_OT_FACE {
                        harfbuzz::Font::from_locator(&handle.handle)?
                    } else {
                        harfbuzz::Font::new(face.face)
                    };

                    let features = match &handle.harfbuzz_features {
                        Some(features) => features
                            .iter()
                            .filter_map(|s| harfbuzz::feature_from_string(s).ok())
                            .collect(),
                        None => self.features.clone(),
                    };

                    *opt_pair = Some(FontPair {
                        face,
                        font: RefCell::new(font),
                        shaped_any: false,
                        presentation: if handle.assume_emoji_presentation {
                            Presentation::Emoji
                        } else {
                            Presentation::Text
                        },
                        features,
                        last_size_and_dpi: RefCell::new(None),
                    });
                }

                if retain_unused_fallback_fonts() && !opt_pair.as_ref().unwrap().shaped_any {
                    let mut unused = self.unused_fonts.borrow_mut();
                    unused.retain(|&idx| idx != font_idx);
                    unused.push_back(font_idx);
                    while unused.len() > MAX_UNUSED_FALLBACK_FONTS {
                        let evicted = unused.pop_front().unwrap();
                        // The current index was moved to the back above, so
                        // eviction cannot borrow the slot currently in use.
                        let mut slot = self.fonts[evicted].borrow_mut();
                        if slot.as_ref().is_some_and(|pair| !pair.shaped_any) {
                            slot.take();
                        }
                    }
                }

                Ok(Some(RefMut::map(opt_pair, |opt_pair| {
                    opt_pair.as_mut().unwrap()
                })))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn do_shape(
        &self,
        mut font_idx: FallbackIdx,
        s: &str,
        font_size: f64,
        dpi: u32,
        no_glyphs: &mut Vec<char>,
        presentation: Option<Presentation>,
        direction: Direction,
        range: Range<usize>,
        presentation_width: Option<&PresentationWidth>,
    ) -> anyhow::Result<Vec<GlyphInfo>> {
        let mut buf = harfbuzz::Buffer::new()?;
        // We deliberately omit setting the script and leave it to harfbuzz
        // to infer from the buffer contents so that it can correctly
        // enable appropriate preprocessing for eg: Hangul.
        // <https://github.com/wezterm/wezterm/issues/1474> and
        // <https://github.com/wezterm/wezterm/issues/1573>
        // buf.set_script(harfbuzz::hb_script_t::HB_SCRIPT_LATIN);
        buf.set_direction(match direction {
            Direction::LeftToRight => harfbuzz::hb_direction_t::HB_DIRECTION_LTR,
            Direction::RightToLeft => harfbuzz::hb_direction_t::HB_DIRECTION_RTL,
        });
        buf.set_language(self.lang);

        buf.add_str(s, range.clone());
        buf.guess_segment_properties();
        buf.set_cluster_level(
            harfbuzz::hb_buffer_cluster_level_t::HB_BUFFER_CLUSTER_LEVEL_MONOTONE_GRAPHEMES,
        );

        let shaped_any;

        // We set this to true when we've run out of fallback fonts to try.
        // In that case, we accept shaper info with codepoint==0 and
        // will use the notdef glyph from the base font.
        let mut no_more_fallbacks = false;

        loop {
            match self.load_fallback(font_idx).context("load_fallback")? {
                Some(mut pair) => {
                    if let Some(p) = presentation {
                        if pair.presentation != p {
                            log::trace!(
                                "wanted presentation is {p:?} != font \
                                     presentation {:?} so skip \
                                     font_idx={font_idx}",
                                pair.presentation
                            );
                            font_idx += 1;
                            continue;
                        }
                    }
                    let point_size = font_size * self.handles[font_idx].scale.unwrap_or(1.);

                    // Tell harfbuzz to recompute important font metrics!
                    if *pair.last_size_and_dpi.borrow() != Some((point_size, dpi)) {
                        let selected_font_size = pair.face.set_font_size(point_size, dpi)?;

                        let pixel_size = if selected_font_size.is_scaled {
                            (point_size * dpi as f64 / 72.) as u32
                        } else {
                            selected_font_size.height as u32
                        };

                        let mut font = pair.font.borrow_mut();

                        if USE_OT_FACE {
                            font.set_ppem(pixel_size, pixel_size);
                            font.set_ptem(point_size as f32);
                            let scale = pixel_size as i32 * 64;
                            font.set_font_scale(scale, scale);
                        } else {
                            // The default hinting flags depend on DPI. A
                            // retained face must refresh them when crossing
                            // that boundary, just like a newly loaded face.
                            let handle = &self.handles[font_idx];
                            let (load_flags, _) = ftwrap::compute_load_flags_from_config(
                                handle.freetype_load_flags,
                                handle.freetype_load_target,
                                handle.freetype_render_target,
                                Some(dpi),
                            );
                            font.set_load_flags(load_flags);
                        }

                        font.font_changed();

                        if USE_OT_FUNCS {
                            font.set_ot_funcs();
                        }
                        pair.last_size_and_dpi
                            .borrow_mut()
                            .replace((point_size, dpi));
                    }

                    let mut font = pair.font.borrow_mut();
                    shaped_any = pair.shaped_any;
                    font.shape(&mut buf, pair.features.as_slice());
                    log::trace!(
                        "shaped font_idx={} {:?} presentation={presentation:?} as: {}",
                        font_idx,
                        &s[range.start..range.end],
                        buf.serialize(Some(&*font))
                    );
                    break;
                }
                None => {
                    for c in s.chars() {
                        no_glyphs.push(c);
                    }

                    if presentation.is_some() {
                        log::debug!(
                            "Ran out of fallback options, retry shape with no presentation"
                        );
                        // Ran out of fallbacks and we have an explicit presentation.
                        // This is a little awkward; we want to record the missing
                        // glyphs so that we can resolve them async, but we also
                        // want to try the current set of fonts without forcing
                        // the presentation match as we might find the results
                        // that way.
                        // Let's restart the shape but pretend that no specific
                        // presentation was used.
                        // We'll probably match the emoji presentation for something,
                        // but might potentially discover the text presentation for
                        // that glyph in a fallback font and swap it out a little
                        // later after a flash of showing the emoji one.
                        return self.do_shape(
                            0,
                            s,
                            font_size,
                            dpi,
                            no_glyphs,
                            None,
                            direction,
                            range,
                            presentation_width,
                        );
                    }

                    // One more go around to pick up the base font and
                    // accept using the notdef glyph from that.
                    no_more_fallbacks = true;
                    font_idx = 0;
                    continue;
                }
            }
        }

        let hb_infos = buf.glyph_infos();
        let positions = buf.glyph_positions();

        let mut cluster = Vec::with_capacity(s.len());
        let mut info_clusters: Vec<Vec<Info>> = Vec::with_capacity(s.len());

        // At this point we have a list of glyphs from the shaper.
        // Each glyph will have `info.cluster` set to the byte index
        // into `s`.  Multiple byte positions can be coalesced into
        // the same `info.cluster` value, representing text that combines
        // into a ligature.
        // It is important for the terminal to understand this relationship
        // because the cell width of that range of text depends on the unicode
        // version at the time that the text was added to the terminal.
        // To calculate the width per glyph:
        // * Make a pass over the clusters to identify the `info.cluster` starting
        //   positions of all of the glyphs
        // * Sort by info.cluster
        // * Dedup
        // * We can now get the byte length of each cluster by looking at the difference
        //   between the `info.cluster` values.
        // * `presentation_width` can be used to resolve the cell width of those
        //   byte ranges.
        // * Then distribute the glyphs across that cell width when assigning them
        //   to a GlyphInfo.
        //
        // A further complication is that the shaper may cluster in surprising ways.
        // For example, the sequence "\u{28}\u{ff9f}" is a single grapheme with
        // width of 2 cells, but harfbuzz returns that as two different clusters.
        //
        // In situations where we know better (eg: when we have presentation_width)
        // we can fixup the cluster information to be based on the cell index for
        // the harfbuzz byte start.
        // <https://github.com/wezterm/wezterm/issues/2572>

        let mut cluster_resolver = ClusterResolver {
            presentation_width,
            ..Default::default()
        };

        cluster_resolver.build(hb_infos, s, &range);
        log::debug!("cluster_resolver: {cluster_resolver:#?}");

        let info_iter = hb_infos.iter().zip(positions.iter()).peekable();
        for (info, pos) in info_iter {
            let cluster_info = match cluster_resolver.get_mut(info.cluster as usize) {
                Some(i) => i,
                None => panic!(
                    "expected cluster info.cluster {} to be in cluster_resolver",
                    info.cluster
                ),
            };
            let len = cluster_info.byte_len;

            let mut info = Info {
                // Take care to use our adjusted cluster_info.start rather
                // than the info.cluster value, otherwise the combination
                // of info.cluster + info.len will be out of bounds later
                // in this code
                cluster: cluster_info.start,
                len,
                codepoint: info.codepoint,
                x_advance: pos.x_advance,
                y_advance: pos.y_advance,
                x_offset: pos.x_offset,
                y_offset: pos.y_offset,
            };
            log::debug!("hb info.cluster {} -> {info:?}", info.cluster);

            if info.codepoint == 0 && !no_more_fallbacks {
                cluster_info.incomplete = true;
            }

            if let Some(ref mut cluster) = info_clusters.last_mut() {
                // Don't fragment runs of unresolved codepoints; they could be a sequence
                // that shapes together in a fallback font.
                if info.codepoint == 0 && !no_more_fallbacks {
                    let prior = cluster.last_mut().unwrap();
                    // This logic essentially merges `info` into `prior` by
                    // extending the length of prior by `info`.
                    // We can only do that if they are contiguous.
                    // Take care, as the shaper may have re-ordered things!
                    if prior.codepoint == 0 || prior.cluster == info.cluster {
                        if prior.cluster + prior.len == info.cluster {
                            // Coalesce with prior
                            prior.len += info.len;
                            continue;
                        } else if info.cluster + info.len == prior.cluster {
                            // We actually precede prior; we must have been
                            // re-ordered by the shaper. Re-arrange and
                            // coalesce
                            std::mem::swap(&mut info, prior);
                            prior.len += info.len;
                            continue;
                        } else if info.cluster + info.len == prior.cluster + prior.len {
                            // Overlaps and coincide with the end of prior; this one folds away.
                            // This can happen with NFD rather than NFC text.
                            // <https://github.com/wezterm/wezterm/issues/2032>
                            continue;
                        }
                        // log::info!("prior={:#?}, info={:#?}", prior, info);
                    }
                }

                // It is important that this bit happens after we've had the
                // opportunity to coalesce runs of unresolved codepoints,
                // otherwise we can produce incorrect shaping
                // <https://github.com/wezterm/wezterm/issues/2482>
                if cluster.last().unwrap().cluster == info.cluster {
                    cluster.push(info);
                    continue;
                }
            }
            info_clusters.push(vec![info]);
        }
        //  log::error!("do_shape: font_idx={} {:?} {:#?}", font_idx, &s[range.clone()], info_clusters);
        log::debug!("font_idx={font_idx} info_clusters: {:#?}", info_clusters);

        // Protect a face that will contribute real output before recursing
        // into missing clusters. Otherwise a long fallback walk could evict
        // this newly successful face before the outer call returns.
        let contributes_directly = info_clusters.iter().any(|infos| {
            !cluster_resolver
                .get(infos[0].cluster)
                .expect("assigned above")
                .incomplete
        });
        if retain_unused_fallback_fonts() && !shaped_any && contributes_directly {
            if let Some(pair) = self.fonts[font_idx].borrow_mut().as_mut() {
                pair.shaped_any = true;
            }
            self.unused_fonts
                .borrow_mut()
                .retain(|&idx| idx != font_idx);
        }

        for infos in &info_clusters {
            let cluster_info = cluster_resolver
                .get(infos[0].cluster)
                .expect("assigned above");
            let sub_range = cluster_info.start..cluster_info.start + cluster_info.byte_len;
            let substr = &s[sub_range.clone()];

            if cluster_info.incomplete {
                // One or more entries didn't have a corresponding glyph,
                // so try a fallback

                /*
                if font_idx == 0 {
                    log::error!("incomplete cluster for text={:?} {:?}", s, info_clusters);
                }
                */

                let first_info = &infos[0];

                let mut shape = match self.do_shape(
                    font_idx + 1,
                    s,
                    font_size,
                    dpi,
                    no_glyphs,
                    presentation,
                    direction,
                    // NOT! substr; this is a coalesced sequence of incomplete clusters!
                    first_info.cluster..first_info.cluster + first_info.len,
                    presentation_width,
                ) {
                    Ok(shape) => Ok(shape),
                    Err(e) => {
                        error!("{:?} for {:?}", e, substr);
                        self.do_shape(
                            0,
                            &make_question_string(substr),
                            font_size,
                            dpi,
                            no_glyphs,
                            presentation,
                            direction,
                            sub_range,
                            presentation_width,
                        )
                    }
                }?;

                cluster.append(&mut shape);
                continue;
            }

            let total_width: f64 = infos.iter().map(|info| info.x_advance as f64).sum();
            let mut remaining_cells = cluster_info.cell_width;

            for info in infos.iter() {
                // Proportional width based on relative pixel dimensions vs. other glyphs in
                // this same cluster
                // Note that weighted_cell_width can legitimately compute as zero here
                // for the case where a combining mark composes over another glyph
                // However, some symbol fonts have broken advance metrics and we don't
                // want those glyphs to end up with zero width, so if this run is zero
                // width then we round up to 1 cell.
                // <https://github.com/wezterm/wezterm/issues/1787>
                let weighted_cell_width = if total_width == 0. {
                    1
                } else {
                    (cluster_info.cell_width as f64 * info.x_advance as f64 / total_width).ceil()
                        as u8
                };
                let weighted_cell_width = weighted_cell_width.min(remaining_cells);
                remaining_cells = remaining_cells.saturating_sub(weighted_cell_width);

                let glyph = make_glyphinfo(substr, weighted_cell_width, font_idx, info);

                cluster.push(glyph);
            }
        }

        if !retain_unused_fallback_fonts() && !shaped_any {
            // Same-binary native control: preserve the original immediate
            // unload policy when the experiment switch is set.
            if let Some(slot) = self.fonts.get(font_idx) {
                let mut slot = slot.borrow_mut();
                if !contributes_directly {
                    slot.take();
                } else if let Some(pair) = slot.as_mut() {
                    pair.shaped_any = true;
                }
            }
        }

        Ok(cluster)
    }
}

impl FontShaper for HarfbuzzShaper {
    fn shape(
        &self,
        text: &str,
        size: f64,
        dpi: u32,
        no_glyphs: &mut Vec<char>,
        presentation: Option<Presentation>,
        direction: Direction,
        range: Option<Range<usize>>,
        presentation_width: Option<&PresentationWidth>,
    ) -> anyhow::Result<Vec<GlyphInfo>> {
        let range = range.unwrap_or(0..text.len());

        log::trace!(
            "shape {range:?} `{}` with presentation={presentation:?}",
            text.escape_debug()
        );
        let start = std::time::Instant::now();
        let result = self.do_shape(
            0,
            text,
            size,
            dpi,
            no_glyphs,
            presentation,
            direction,
            range,
            presentation_width,
        );
        metrics::histogram!("shape.harfbuzz").record(start.elapsed());
        /*
        if let Ok(glyphs) = &result {
            for g in glyphs {
                if g.font_idx > 0 {
                    log::error!("{:#?}", result);
                    break;
                }
            }
        }
        */
        result
    }

    fn metrics_for_idx(&self, font_idx: usize, size: f64, dpi: u32) -> anyhow::Result<FontMetrics> {
        let mut pair = self
            .load_fallback(font_idx)?
            .ok_or_else(|| anyhow!("metrics_for_idx: there is no font with idx={font_idx}!?"))?;

        let key = MetricsKey {
            font_idx,
            size: NotNan::new(size).context("font size must not be NaN")?,
            dpi,
        };
        if let Some(metrics) = self.metrics.borrow().get(&key) {
            return Ok(*metrics);
        }

        let scale = self.handles[font_idx].scale.unwrap_or(1.);

        // A metrics query changes the same FT_Face used by HarfBuzz. Force
        // the next shape to restore its requested size and call font_changed,
        // even when it repeats the size of the previous shape.
        // https://harfbuzz.github.io/harfbuzz-hb-ft.html#hb-ft-font-changed
        pair.last_size_and_dpi.borrow_mut().take();
        let selected_size = pair.face.set_font_size(size * scale, dpi)?;
        let y_scale = unsafe { (*(*pair.face.face).size).metrics.y_scale.to_num::<f64>() };
        let mut metrics = FontMetrics {
            cell_height: PixelLength::new(selected_size.height),
            cell_width: PixelLength::new(selected_size.width),
            // Note: face.face.descender is useless, we have to go through
            // face.face.size.metrics to get to the real descender!
            descender: PixelLength::new(unsafe {
                (*(*pair.face.face).size).metrics.descender.f26d6().to_num()
            }),
            underline_thickness: PixelLength::new(
                unsafe { (*pair.face.face).underline_thickness as f64 } * y_scale / 64.,
            ),
            underline_position: PixelLength::new(
                unsafe { (*pair.face.face).underline_position as f64 } * y_scale / 64.,
            ),
            cap_height_ratio: selected_size.cap_height_to_height_ratio,
            cap_height: selected_size.cap_height.map(PixelLength::new),
            is_scaled: selected_size.is_scaled,
            presentation: pair.presentation,
            force_y_adjust: PixelLength::new(0.),
        };

        // When the user has overridden the scale, we need to stash a y-adjustment
        // so that the glyphs are better vertically aligned
        // <https://github.com/wezterm/wezterm/issues/1803>
        if scale != 1.0 && metrics.is_scaled {
            let diff = metrics.descender - (metrics.descender / scale);
            metrics.force_y_adjust = diff;
        }

        self.metrics.borrow_mut().insert(key, metrics);

        log::trace!(
            "metrics_for_idx={}, size={}, dpi={} -> {:?}",
            font_idx,
            size,
            dpi,
            metrics
        );

        Ok(metrics)
    }

    fn metrics(&self, size: f64, dpi: u32) -> anyhow::Result<FontMetrics> {
        // Returns the metrics for the selected font... but look out
        // for implausible sizes.
        // Ideally we wouldn't need this, but in the event that a user
        // has a wonky configuration we don't want to pick something
        // like a bitmap emoji font for the metrics or well end up
        // with crazy huge cells.
        // We do a sniff test based on the theoretical pixel height for
        // the supplied size+dpi.
        // If a given fallback slot deviates from the theoretical size
        // by too much we'll skip to the next slot.
        let theoretical_height = size * dpi as f64 / 72.0;
        let mut metrics_idx = 0;
        log::trace!(
            "compute metrics across these handles for size={}, dpi={},
             theoretical pixel height {}: {:?}",
            size,
            dpi,
            theoretical_height,
            self.handles
        );
        while let Ok(Some(mut pair)) = self.load_fallback(metrics_idx) {
            pair.last_size_and_dpi.borrow_mut().take();
            let selected_size = pair
                .face
                .set_font_size(size * self.handles[metrics_idx].scale.unwrap_or(1.), dpi)?;
            let diff = (theoretical_height - selected_size.height).abs();
            let factor = diff / theoretical_height;
            if factor < 2.0 {
                log::trace!(
                    "idx {} cell_height is {}, which is {} away from theoretical
                     height (factor {}). Seems good enough",
                    metrics_idx,
                    selected_size.height,
                    diff,
                    factor
                );
                break;
            }
            log::trace!(
                "skip idx {} because diff={} factor={} theoretical_height={} cell_height={}",
                metrics_idx,
                diff,
                factor,
                theoretical_height,
                selected_size.height
            );

            if metrics_idx + 1 >= self.handles.len() {
                log::warn!(
                    "metrics: I wanted to skip idx {} because diff={} factor={} \
                    theoretical_height={} cell_height={}, but there are no more \
                    fallback fonts. Metrics will likely be crazy.",
                    metrics_idx,
                    diff,
                    factor,
                    theoretical_height,
                    selected_size.height
                );
                break;
            }

            metrics_idx += 1;
        }

        self.metrics_for_idx(metrics_idx, size, dpi)
    }
}

#[derive(Debug)]
struct ClusterInfo {
    start: usize,
    byte_len: usize,
    cell_width: u8,
    incomplete: bool,
}

#[derive(Default, Debug)]
struct ClusterResolver<'a> {
    map: HashMap<usize, ClusterInfo>,
    presentation_width: Option<&'a PresentationWidth<'a>>,
    start_by_cell_idx: HashMap<usize, usize>,
}

impl<'a> ClusterResolver<'a> {
    pub fn build(&mut self, hb_infos: &[harfbuzz::hb_glyph_info_t], s: &str, range: &Range<usize>) {
        #[derive(PartialOrd, Ord, Eq, PartialEq, Copy, Clone)]
        struct Item {
            cell_idx: Option<usize>,
            start: usize,
        }

        let mut map = HashMap::new();

        for info in hb_infos.iter() {
            let start = info.cluster as usize;

            // Collect the cell index, which is the "true cluster"
            // position from our perspective: the start of the grapheme
            let cell_idx = match self.presentation_width {
                Some(pw) => {
                    let cell_idx = pw.byte_to_cell_idx(start);

                    let entry = self.start_by_cell_idx.entry(cell_idx).or_insert(start);
                    *entry = (*entry).min(start);

                    Some(cell_idx)
                }
                None => None,
            };

            map.entry(start).or_insert_with(|| Item { start, cell_idx });
        }

        let mut cluster_starts: Vec<Item> = map.into_values().collect();
        cluster_starts.sort();

        // If we have multiple entries with the same starting cell index,
        // remove the duplicates.  Don't do this for `None` as that will
        // falsely remove valid cluster locations in the case where
        // we have no presentation_width information.
        cluster_starts.dedup_by(|a, b| match (a.cell_idx, b.cell_idx) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        });

        let mut iter = cluster_starts.iter().peekable();
        while let Some(item) = iter.next().copied() {
            let start = item.start;
            let next_start = iter.peek().map(|&&s| s.start).unwrap_or(range.end);
            let byte_len = next_start - start;
            let cell_width = match self.presentation_width {
                Some(p) => p.num_cells(start..next_start),
                None => unicode_column_width(&s[start..next_start], None) as u8,
            };
            self.map.entry(start).or_insert_with(|| ClusterInfo {
                start,
                byte_len,
                cell_width,
                incomplete: false,
            });
        }
    }

    pub fn get_mut(&mut self, start: usize) -> Option<&mut ClusterInfo> {
        match self.presentation_width {
            Some(pw) => {
                let cell_idx = pw.byte_to_cell_idx(start);
                let actual_start = self.start_by_cell_idx.get(&cell_idx)?;
                self.map.get_mut(actual_start)
            }
            None => self.map.get_mut(&start),
        }
    }

    pub fn get(&self, start: usize) -> Option<&ClusterInfo> {
        match self.presentation_width {
            Some(pw) => {
                let cell_idx = pw.byte_to_cell_idx(start);
                let actual_start = self.start_by_cell_idx.get(&cell_idx)?;
                self.map.get(actual_start)
            }
            None => self.map.get(&start),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::FontDatabase;
    use config::FontAttributes;

    fn fallback_test_handles(count: usize) -> Vec<ParsedFont> {
        let db = FontDatabase::with_built_in().unwrap();
        let handle = db
            .resolve(
                &FontAttributes {
                    family: "JetBrains Mono".into(),
                    stretch: Default::default(),
                    weight: Default::default(),
                    is_fallback: false,
                    is_synthetic: false,
                    style: Default::default(),
                    freetype_load_flags: None,
                    freetype_load_target: None,
                    freetype_render_target: None,
                    harfbuzz_features: None,
                    scale: None,
                    assume_emoji_presentation: None,
                },
                14,
            )
            .unwrap()
            .clone();
        vec![handle; count]
    }

    #[test]
    fn invalid_font_sizes_return_errors_without_poisoning_cached_faces() {
        let handles = fallback_test_handles(1);
        let config = config::configuration();
        let shaper = HarfbuzzShaper::new(&config, &handles).unwrap();
        let fresh = HarfbuzzShaper::new(&config, &handles).unwrap();
        let shape = |shaper: &HarfbuzzShaper, size| {
            shaper.shape(
                "WZ ffi e\u{301}",
                size,
                72,
                &mut Vec::new(),
                None,
                Direction::LeftToRight,
                None,
                None,
            )
        };
        let expected = shape(&fresh, 10.).unwrap();
        assert_eq!(shape(&shaper, 10.).unwrap(), expected);
        for size in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, f64::MAX, 0., -1.] {
            assert!(shape(&shaper, size).is_err(), "shape accepted {size}");
            assert!(shaper.metrics(size, 72).is_err(), "metrics accepted {size}");
            assert!(
                shaper.metrics_for_idx(0, size, 72).is_err(),
                "indexed metrics accepted {size}"
            );
            assert_eq!(shape(&shaper, 10.).unwrap(), expected);
        }
    }

    #[test]
    fn metric_queries_at_other_sizes_do_not_change_subsequent_shaping() {
        let handles = fallback_test_handles(1);
        let config = config::configuration();
        for general_metrics in [false, true] {
            let shaper = HarfbuzzShaper::new(&config, &handles).unwrap();
            shaper
                .shape(
                    "a",
                    10.,
                    72,
                    &mut Vec::new(),
                    None,
                    Direction::LeftToRight,
                    None,
                    None,
                )
                .unwrap();
            if general_metrics {
                shaper.metrics(26., 144).unwrap();
            } else {
                shaper.metrics_for_idx(0, 26., 144).unwrap();
            }
            let fresh = HarfbuzzShaper::new(&config, &handles).unwrap();
            let text = "WZ ffi e\u{301}";
            assert_eq!(
                shaper
                    .shape(
                        text,
                        10.,
                        72,
                        &mut Vec::new(),
                        None,
                        Direction::LeftToRight,
                        None,
                        None
                    )
                    .unwrap(),
                fresh
                    .shape(
                        text,
                        10.,
                        72,
                        &mut Vec::new(),
                        None,
                        Direction::LeftToRight,
                        None,
                        None
                    )
                    .unwrap(),
                "metric-only size selection must invalidate the previous Harfbuzz size"
            );
        }
    }

    #[test]
    fn unsuccessful_fallback_faces_are_reused_with_fresh_shape_parity() {
        let handles = fallback_test_handles(3);
        let config = config::configuration();
        let shaper = HarfbuzzShaper::new(&config, &handles).unwrap();
        for (size, dpi, direction) in [
            (10., 72, Direction::LeftToRight),
            (10., 72, Direction::LeftToRight),
            (14., 96, Direction::RightToLeft),
            (12., 144, Direction::LeftToRight),
            (10., 72, Direction::LeftToRight),
        ] {
            for text in ["\u{10ffff}", "ffi e\u{301} \u{10ffff} =>"] {
                let mut missing = Vec::new();
                let actual = shaper
                    .shape(text, size, dpi, &mut missing, None, direction, None, None)
                    .unwrap();
                let fresh = HarfbuzzShaper::new(&config, &handles).unwrap();
                let mut fresh_missing = Vec::new();
                let expected = fresh
                    .shape(
                        text,
                        size,
                        dpi,
                        &mut fresh_missing,
                        None,
                        direction,
                        None,
                        None,
                    )
                    .unwrap();
                assert_eq!(actual, expected);
                assert_eq!(missing, fresh_missing);
                assert!(!missing.is_empty(), "exercise actual unresolved fallback");
                for slot in shaper.fonts.iter().skip(1) {
                    let slot = slot.borrow();
                    let pair = slot
                        .as_ref()
                        .expect("unsuccessful fallback must remain loaded between clusters");
                    assert!(!pair.shaped_any);
                    assert_eq!(*pair.last_size_and_dpi.borrow(), Some((size, dpi)));
                }
            }
        }
    }

    #[test]
    fn unsuccessful_fallback_residency_is_bounded_and_successful_faces_stay_loaded() {
        let handles = fallback_test_handles(MAX_UNUSED_FALLBACK_FONTS + 5);
        let config = config::configuration();
        let shaper = HarfbuzzShaper::new(&config, &handles).unwrap();
        let text = "\u{10ffff} ffi";
        let mut missing = Vec::new();
        let expected = shaper
            .shape(
                text,
                10.,
                72,
                &mut missing,
                None,
                Direction::LeftToRight,
                None,
                None,
            )
            .unwrap();
        assert!(shaper.fonts[0].borrow().as_ref().unwrap().shaped_any);
        let promoted = *shaper.unused_fonts.borrow().front().unwrap();
        assert!(promoted > 0);
        let promoted_output = shaper
            .do_shape(
                promoted,
                "ffi",
                10.,
                72,
                &mut Vec::new(),
                None,
                Direction::LeftToRight,
                0..3,
                None,
            )
            .unwrap();
        assert!(!promoted_output.is_empty());
        assert!(promoted_output
            .iter()
            .all(|glyph| glyph.font_idx == promoted));
        assert!(!shaper.unused_fonts.borrow().contains(&promoted));
        for _ in 0..3 {
            let mut actual_missing = Vec::new();
            assert_eq!(
                shaper
                    .shape(
                        text,
                        10.,
                        72,
                        &mut actual_missing,
                        None,
                        Direction::LeftToRight,
                        None,
                        None
                    )
                    .unwrap(),
                expected
            );
            assert_eq!(actual_missing, missing);
            assert!(shaper.fonts[promoted].borrow().as_ref().unwrap().shaped_any);
            assert!(shaper.fonts[0].borrow().as_ref().unwrap().shaped_any);
            let unused_resident = shaper
                .fonts
                .iter()
                .filter(|slot| slot.borrow().as_ref().is_some_and(|pair| !pair.shaped_any))
                .count();
            assert!(unused_resident <= MAX_UNUSED_FALLBACK_FONTS);
            assert_eq!(unused_resident, shaper.unused_fonts.borrow().len());
        }
    }

    #[test]
    fn ligatures() {
        let _ = env_logger::Builder::new()
            .is_test(true)
            .filter_level(log::LevelFilter::Trace)
            .try_init();

        let db = FontDatabase::with_built_in().unwrap();
        let handle = db
            .resolve(
                &FontAttributes {
                    family: "JetBrains Mono".into(),
                    stretch: Default::default(),
                    weight: Default::default(),
                    is_fallback: false,
                    is_synthetic: false,
                    style: Default::default(),
                    freetype_load_flags: None,
                    freetype_load_target: None,
                    freetype_render_target: None,
                    harfbuzz_features: None,
                    scale: None,
                    assume_emoji_presentation: None,
                },
                14,
            )
            .unwrap()
            .clone();

        let config = config::configuration();

        let shaper = HarfbuzzShaper::new(&config, &[handle]).unwrap();
        {
            let mut no_glyphs = vec![];
            let info = shaper
                .shape(
                    "abc",
                    10.,
                    72,
                    &mut no_glyphs,
                    None,
                    Direction::LeftToRight,
                    None,
                    None,
                )
                .unwrap();
            assert!(no_glyphs.is_empty(), "{:?}", no_glyphs);
            k9::snapshot!(
                info,
                r#"
[
    GlyphInfo {
        text: "a",
        only_char: Some(
            'a',
        ),
        is_space: false,
        num_cells: 1,
        cluster: 0,
        font_idx: 0,
        glyph_pos: 189,
        x_advance: 6.0,
        y_advance: 0.0,
        x_offset: 0.0,
        y_offset: 0.0,
    },
    GlyphInfo {
        text: "b",
        only_char: Some(
            'b',
        ),
        is_space: false,
        num_cells: 1,
        cluster: 1,
        font_idx: 0,
        glyph_pos: 214,
        x_advance: 6.0,
        y_advance: 0.0,
        x_offset: 0.0,
        y_offset: 0.0,
    },
    GlyphInfo {
        text: "c",
        only_char: Some(
            'c',
        ),
        is_space: false,
        num_cells: 1,
        cluster: 2,
        font_idx: 0,
        glyph_pos: 215,
        x_advance: 6.0,
        y_advance: 0.0,
        x_offset: 0.0,
        y_offset: 0.0,
    },
]
"#
            );
        }
        {
            let mut no_glyphs = vec![];
            let info = shaper
                .shape(
                    "<",
                    10.,
                    72,
                    &mut no_glyphs,
                    None,
                    Direction::LeftToRight,
                    None,
                    None,
                )
                .unwrap();
            assert!(no_glyphs.is_empty(), "{:?}", no_glyphs);
            k9::snapshot!(
                info,
                r#"
[
    GlyphInfo {
        text: "<",
        only_char: Some(
            '<',
        ),
        is_space: false,
        num_cells: 1,
        cluster: 0,
        font_idx: 0,
        glyph_pos: 1052,
        x_advance: 6.0,
        y_advance: 0.0,
        x_offset: 0.0,
        y_offset: 0.0,
    },
]
"#
            );
        }
        {
            // This is a ligatured sequence, but you wouldn't know
            // from this info :-/
            let mut no_glyphs = vec![];
            let info = shaper
                .shape(
                    "<-",
                    10.,
                    72,
                    &mut no_glyphs,
                    None,
                    Direction::LeftToRight,
                    None,
                    None,
                )
                .unwrap();
            assert!(no_glyphs.is_empty(), "{:?}", no_glyphs);
            k9::snapshot!(
                info,
                r#"
[
    GlyphInfo {
        text: "<",
        only_char: Some(
            '<',
        ),
        is_space: false,
        num_cells: 1,
        cluster: 0,
        font_idx: 0,
        glyph_pos: 1742,
        x_advance: 6.0,
        y_advance: 0.0,
        x_offset: 0.0,
        y_offset: 0.0,
    },
    GlyphInfo {
        text: "-",
        only_char: Some(
            '-',
        ),
        is_space: false,
        num_cells: 1,
        cluster: 1,
        font_idx: 0,
        glyph_pos: 1588,
        x_advance: 6.0,
        y_advance: 0.0,
        x_offset: 0.0,
        y_offset: 0.0,
    },
]
"#
            );
        }
        {
            let mut no_glyphs = vec![];
            let info = shaper
                .shape(
                    "<--",
                    10.,
                    72,
                    &mut no_glyphs,
                    None,
                    Direction::LeftToRight,
                    None,
                    None,
                )
                .unwrap();
            assert!(no_glyphs.is_empty(), "{:?}", no_glyphs);
            k9::snapshot!(
                info,
                r#"
[
    GlyphInfo {
        text: "<",
        only_char: Some(
            '<',
        ),
        is_space: false,
        num_cells: 1,
        cluster: 0,
        font_idx: 0,
        glyph_pos: 1742,
        x_advance: 6.0,
        y_advance: 0.0,
        x_offset: 0.0,
        y_offset: 0.0,
    },
    GlyphInfo {
        text: "-",
        only_char: Some(
            '-',
        ),
        is_space: false,
        num_cells: 1,
        cluster: 1,
        font_idx: 0,
        glyph_pos: 1742,
        x_advance: 6.0,
        y_advance: 0.0,
        x_offset: 0.0,
        y_offset: 0.0,
    },
    GlyphInfo {
        text: "-",
        only_char: Some(
            '-',
        ),
        is_space: false,
        num_cells: 1,
        cluster: 2,
        font_idx: 0,
        glyph_pos: 1589,
        x_advance: 6.0,
        y_advance: 0.0,
        x_offset: 0.0,
        y_offset: 0.0,
    },
]
"#
            );
        }

        {
            let mut no_glyphs = vec![];
            let info = shaper
                .shape(
                    "x x",
                    10.,
                    72,
                    &mut no_glyphs,
                    None,
                    Direction::LeftToRight,
                    None,
                    None,
                )
                .unwrap();
            assert!(no_glyphs.is_empty(), "{:?}", no_glyphs);
            k9::snapshot!(
                info,
                r#"
[
    GlyphInfo {
        text: "x",
        only_char: Some(
            'x',
        ),
        is_space: false,
        num_cells: 1,
        cluster: 0,
        font_idx: 0,
        glyph_pos: 367,
        x_advance: 6.0,
        y_advance: 0.0,
        x_offset: 0.0,
        y_offset: 0.0,
    },
    GlyphInfo {
        text: " ",
        only_char: Some(
            ' ',
        ),
        is_space: true,
        num_cells: 1,
        cluster: 1,
        font_idx: 0,
        glyph_pos: 958,
        x_advance: 6.0,
        y_advance: 0.0,
        x_offset: 0.0,
        y_offset: 0.0,
    },
    GlyphInfo {
        text: "x",
        only_char: Some(
            'x',
        ),
        is_space: false,
        num_cells: 1,
        cluster: 2,
        font_idx: 0,
        glyph_pos: 367,
        x_advance: 6.0,
        y_advance: 0.0,
        x_offset: 0.0,
        y_offset: 0.0,
    },
]
"#
            );
        }

        {
            let mut no_glyphs = vec![];
            let info = shaper
                .shape(
                    "x\u{3000}x",
                    10.,
                    72,
                    &mut no_glyphs,
                    None,
                    Direction::LeftToRight,
                    None,
                    None,
                )
                .unwrap();
            assert!(no_glyphs.is_empty(), "{:?}", no_glyphs);
            k9::snapshot!(
                info,
                r#"
[
    GlyphInfo {
        text: "x",
        only_char: Some(
            'x',
        ),
        is_space: false,
        num_cells: 1,
        cluster: 0,
        font_idx: 0,
        glyph_pos: 367,
        x_advance: 6.0,
        y_advance: 0.0,
        x_offset: 0.0,
        y_offset: 0.0,
    },
    GlyphInfo {
        text: "\u{3000}",
        only_char: Some(
            '\u{3000}',
        ),
        is_space: false,
        num_cells: 2,
        cluster: 1,
        font_idx: 0,
        glyph_pos: 958,
        x_advance: 10.0,
        y_advance: 0.0,
        x_offset: 0.0,
        y_offset: 0.0,
    },
    GlyphInfo {
        text: "x",
        only_char: Some(
            'x',
        ),
        is_space: false,
        num_cells: 1,
        cluster: 4,
        font_idx: 0,
        glyph_pos: 367,
        x_advance: 6.0,
        y_advance: 0.0,
        x_offset: 0.0,
        y_offset: 0.0,
    },
]
"#
            );
        }
    }
}
