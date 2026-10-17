//! Rasterization cost per glyph (ft-yccm0.4.3.1): FreeType against CoreText
//! at the image-parity corpus size (13 pt, 144 dpi), on the built-in
//! JetBrains Mono's printable ASCII and, on macOS, on Apple Color Emoji's
//! Emoticons block (part of the T0 emoji set). Each iteration rasterizes every
//! glyph of its set once, and throughput is reported per glyph. CoreText is
//! constructed directly, never through the FreeType kill-switch fallback, so
//! a CoreText row always measures CoreText.
//!
//! `cargo bench -p frankenterm-font --bench rasterize_glyph`

use config::FontAttributes;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use frankenterm_font::db::FontDatabase;
use frankenterm_font::parser::ParsedFont;
use frankenterm_font::rasterizer::freetype::FreeTypeRasterizer;
use frankenterm_font::rasterizer::FontRasterizer;
use frankenterm_font::shaper::harfbuzz::HarfbuzzShaper;
use frankenterm_font::shaper::FontShaper;
use std::hint::black_box;
use wezterm_bidi::Direction;

const SIZE_PT: f64 = 13.0;
const DPI: u32 = 144;

fn jetbrains_mono() -> ParsedFont {
    let db = FontDatabase::with_built_in().expect("built-in fonts");
    db.resolve(
        &FontAttributes {
            family: "JetBrains Mono".into(),
            ..FontAttributes::default()
        },
        26,
    )
    .expect("JetBrains Mono is built in")
    .clone()
}

/// The distinct glyphs HarfBuzz shapes `text` into with `font`.
fn glyphs(font: &ParsedFont, text: &str) -> Vec<u32> {
    let config = config::configuration();
    let shaper = HarfbuzzShaper::new(&config, std::slice::from_ref(font)).expect("shaper");
    let mut glyphs: Vec<u32> = shaper
        .shape(
            text,
            SIZE_PT,
            DPI,
            &mut Vec::new(),
            None,
            Direction::LeftToRight,
            None,
            None,
        )
        .expect("shape")
        .iter()
        .map(|info| info.glyph_pos)
        .filter(|&glyph| glyph != 0)
        .collect();
    glyphs.sort_unstable();
    glyphs.dedup();
    assert!(!glyphs.is_empty(), "no glyphs for {text:?}");
    glyphs
}

/// The rasterizers to compare for `font`. One that cannot rasterize the
/// set's first glyph (FreeType on an sbix-only face, say) is left out with a
/// note rather than measured failing.
fn rasterizers(font: &ParsedFont, probe: u32) -> Vec<(&'static str, Box<dyn FontRasterizer>)> {
    let mut list: Vec<(&'static str, Box<dyn FontRasterizer>)> = Vec::new();
    match FreeTypeRasterizer::from_locator(font, Default::default()) {
        Ok(freetype) => list.push(("freetype", Box::new(freetype))),
        Err(err) => eprintln!("freetype: not measured: {err:#}"),
    }
    #[cfg(target_os = "macos")]
    match frankenterm_font::rasterizer::coretext::CoreTextRasterizer::from_locator(font) {
        Ok(coretext) => list.push(("coretext", Box::new(coretext))),
        Err(err) => eprintln!("coretext: not measured: {err:#}"),
    }
    list.retain(
        |(name, rasterizer)| match rasterizer.rasterize_glyph(probe, SIZE_PT, DPI) {
            Ok(_) => true,
            Err(err) => {
                eprintln!("{name}: not measured: glyph {probe}: {err:#}");
                false
            }
        },
    );
    list
}

fn bench_set(c: &mut Criterion, group_name: &str, font: &ParsedFont, glyphs: &[u32]) {
    let mut group = c.benchmark_group(group_name);
    group.throughput(Throughput::Elements(glyphs.len() as u64));
    for (name, rasterizer) in rasterizers(font, glyphs[0]) {
        group.bench_function(BenchmarkId::from_parameter(name), |b| {
            b.iter(|| {
                for &glyph in glyphs {
                    black_box(
                        rasterizer
                            .rasterize_glyph(black_box(glyph), SIZE_PT, DPI)
                            .expect("rasterize"),
                    );
                }
            });
        });
    }
    group.finish();
}

fn ascii(c: &mut Criterion) {
    let font = jetbrains_mono();
    let text: String = (0x21u8..0x7f).map(char::from).collect();
    let glyphs = glyphs(&font, &text);
    bench_set(c, "rasterize_glyph/jetbrains_mono_ascii", &font, &glyphs);
}

#[cfg(target_os = "macos")]
fn emoji(c: &mut Criterion) {
    use frankenterm_font::locator::{FontDataHandle, FontDataSource, FontOrigin};
    let font = ParsedFont::from_locator(&FontDataHandle {
        source: FontDataSource::OnDisk("/System/Library/Fonts/Apple Color Emoji.ttc".into()),
        index: 0,
        variation: 0,
        origin: FontOrigin::CoreText,
        coverage: None,
    })
    .expect("Apple Color Emoji");
    let text: String = (0x1F600u32..=0x1F64F).filter_map(char::from_u32).collect();
    let glyphs = glyphs(&font, &text);
    bench_set(
        c,
        "rasterize_glyph/apple_color_emoji_emoticons",
        &font,
        &glyphs,
    );
}

#[cfg(not(target_os = "macos"))]
fn emoji(_: &mut Criterion) {}

criterion_group!(benches, ascii, emoji);
criterion_main!(benches);
