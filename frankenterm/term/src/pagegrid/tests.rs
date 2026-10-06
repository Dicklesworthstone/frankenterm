//! B3.2 tests: the ADR section 3 layouts, exact legacy round trips, the
//! rich-style table and its SGR cache hook, the grapheme arena, the T0
//! pattern, page reset, and op-sequence properties checked against both
//! the page invariants and the legacy `Line` the page must reproduce.

#![allow(clippy::vec_box)]

use super::cell::{classify, style_attributes};
use super::grapheme::{GraphemeArena, MAX_GRAPHEME_BYTES, STD_ARENA_BYTES};
use super::style::STD_INDEX_SLOTS;
use super::*;
use crate::color::{ColorAttribute, SrgbaTuple};
use frankenterm_cell::image::{ImageCell, ImageData, ImageDataType, TextureCoordinate};
use frankenterm_cell::{
    Blink, Cell, CellAttributes, Hyperlink, Intensity, SemanticType, Underline, VerticalAlign,
};
use frankenterm_surface::line::Line;
use proptest::prelude::*;
use std::sync::Arc;

fn truecolor(n: u32) -> ColorAttribute {
    ColorAttribute::TrueColorWithDefaultFallback(SrgbaTuple(
        (n % 4096) as f32 / 4096.0,
        (n / 4096) as f32 / 4096.0,
        0.5,
        1.0,
    ))
}

fn rich(n: u32) -> RichStyle {
    RichStyle {
        attrs: (n % 7) as u16,
        fg: truecolor(n),
        bg: ColorAttribute::Default,
        underline_color: ColorAttribute::Default,
    }
}

fn xorshift(mut state: u64) -> u64 {
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    state
}

#[test]
fn packed_cell_bits_sit_where_the_adr_puts_them() {
    assert_eq!(std::mem::size_of::<PackedCell>(), 8);
    let blank = PackedCell::BLANK;
    assert_eq!(blank.with_codepoint(0x10_FFFF).bits(), 0x10_FFFF);
    assert_eq!(blank.with_grapheme(true).bits(), 1 << 21);
    assert_eq!(blank.with_wide(true).bits(), 1 << 22);
    assert_eq!(blank.with_hidden(true).bits(), 1 << 23);
    assert_eq!(blank.with_semantic(SemanticType::Input).bits(), 1 << 24);
    assert_eq!(blank.with_semantic(SemanticType::Prompt).bits(), 2 << 24);
    assert_eq!(blank.with_hyperlink(true).bits(), 1 << 26);
    assert_eq!(blank.with_image(true).bits(), 1 << 27);
    assert_eq!(
        blank.with_style(CellStyle::Rich(u32::MAX)).bits(),
        (1 << 28) | (0xFFFF_FFFF << 29)
    );
    assert_eq!(blank.with_wrapped(true).bits(), 1 << 61);
    assert_eq!(PackedCell::RESERVED, 0b11 << 62);

    // Inline style from its low bit: attrs 14, fg 9, bg 9.
    let style = InlineStyle::new(0x3FFF, 256, 256);
    assert_eq!(style.bits(), 0x3FFF | (256 << 14) | (256 << 23));
    let cell = blank.with_style(CellStyle::Inline(style));
    assert_eq!(cell.bits(), u64::from(style.bits()) << 29);
    assert_eq!(cell.style(), CellStyle::Inline(style));
    assert!(!cell.is_rich() && !cell.is_wrapped());

    // Builders replace a field without touching its neighbours.
    let full = PackedCell::from_bits(!PackedCell::RESERVED);
    assert_eq!(full.with_codepoint(0).codepoint(), 0);
    assert_eq!(
        full.with_codepoint(0).bits(),
        !PackedCell::RESERVED & !0x1F_FFFF
    );
    assert_eq!(
        full.with_semantic(SemanticType::Output).bits(),
        !PackedCell::RESERVED & !(0b11 << 24)
    );
}

#[test]
fn row_header_bits_sit_where_the_adr_puts_them() {
    assert_eq!(RowHeader::with_slot(u32::MAX).bits(), 0xFFFF_FFFF);
    assert_eq!(
        RowHeader::default().with_len(0x1_FFFF).bits(),
        0x1_FFFF << 32
    );
    let flags = [
        (RowHeader::DIRTY, 49),
        (RowHeader::STYLED, 50),
        (RowHeader::GRAPHEME, 51),
        (RowHeader::HYPERLINK, 52),
        (RowHeader::IMAGE, 53),
        (RowHeader::SEMANTIC, 54),
        (RowHeader::BIDI_ENABLED, 55),
        (RowHeader::RTL, 56),
        (RowHeader::AUTO_DETECT_DIRECTION, 57),
        (RowHeader::DOUBLE_WIDTH, 58),
        (RowHeader::DOUBLE_HEIGHT_TOP, 59),
        (RowHeader::DOUBLE_HEIGHT_BOTTOM, 60),
        (RowHeader::LEGACY_FORM_C, 61),
    ];
    for &(flag, bit) in &flags {
        assert_eq!(flag, 1 << bit);
    }
    assert_eq!(RowHeader::RESERVED, 0b11 << 62);
    let header = RowHeader::with_slot(7)
        .with_len(121)
        .with_flags(RowHeader::RTL, true);
    assert_eq!((header.slot(), header.len()), (7, 121));
    assert!(header.has(RowHeader::RTL) && !header.has(RowHeader::DIRTY));
}

#[test]
fn page_geometry_matches_the_adr() {
    assert_eq!(rows_per_page(120), 270);
    assert_eq!(rows_per_page(80), 404);
    assert_eq!(rows_per_page(u16::MAX), 1);
    let page = Page::standard(120, 1);
    assert_eq!(page.capacity(), 270);
    // [headers: 270][seqnos: 270][cells: 270 x 121] = 33,210 words.
    assert_eq!(page.buffer_words(), 33_210);
    assert!(page.is_clean());
    page.check_invariants().unwrap();
}

fn intensity(n: u8) -> Intensity {
    match n % 3 {
        0 => Intensity::Normal,
        1 => Intensity::Bold,
        _ => Intensity::Half,
    }
}

fn underline(n: u8) -> Underline {
    match n % 6 {
        0 => Underline::None,
        1 => Underline::Single,
        2 => Underline::Double,
        3 => Underline::Curly,
        4 => Underline::Dotted,
        _ => Underline::Dashed,
    }
}

fn blink(n: u8) -> Blink {
    match n % 3 {
        0 => Blink::None,
        1 => Blink::Slow,
        _ => Blink::Rapid,
    }
}

fn vertical_align(n: u8) -> VerticalAlign {
    match n % 3 {
        0 => VerticalAlign::BaseLine,
        1 => VerticalAlign::SuperScript,
        _ => VerticalAlign::SubScript,
    }
}

fn semantic(n: u8) -> SemanticType {
    match n % 3 {
        0 => SemanticType::Output,
        1 => SemanticType::Input,
        _ => SemanticType::Prompt,
    }
}

/// Every attribute combination, under default, palette and true colours,
/// survives classify and materialize exactly; palette styles stay inline.
#[test]
fn legacy_styles_round_trip_exactly() {
    let colors = [
        ColorAttribute::Default,
        ColorAttribute::PaletteIndex(0),
        ColorAttribute::PaletteIndex(255),
        truecolor(9),
        ColorAttribute::TrueColorWithPaletteFallback(SrgbaTuple(0.1, 0.2, 0.3, 1.0), 4),
    ];
    let mut table = RichStyleTable::new();
    let mut checked = 0;
    for bits in 0..(3 * 6 * 3 * 3 * 32) {
        let mut attrs = CellAttributes::default();
        attrs
            .set_intensity(intensity((bits % 3) as u8))
            .set_underline(underline((bits / 3 % 6) as u8))
            .set_blink(blink((bits / 18 % 3) as u8))
            .set_vertical_align(vertical_align((bits / 54 % 3) as u8));
        let flags = bits / 162;
        attrs
            .set_italic(flags & 1 != 0)
            .set_reverse(flags & 2 != 0)
            .set_strikethrough(flags & 4 != 0)
            .set_invisible(flags & 8 != 0)
            .set_overline(flags & 16 != 0);
        for (i, &fg) in colors.iter().enumerate() {
            let bg = colors[(i + bits as usize) % colors.len()];
            let underline_color = if bits % 11 == 0 {
                truecolor(3)
            } else {
                ColorAttribute::Default
            };
            let mut attrs = attrs.clone();
            attrs
                .set_foreground(fg)
                .set_background(bg)
                .set_underline_color(underline_color);
            let style = match classify(&attrs) {
                StyleClass::Inline(inline) => {
                    let palette = |c: ColorAttribute| {
                        matches!(c, ColorAttribute::Default | ColorAttribute::PaletteIndex(_))
                    };
                    assert!(
                        palette(fg) && palette(bg) && underline_color == ColorAttribute::Default
                    );
                    CellStyle::Inline(inline)
                }
                StyleClass::Rich(rich) => CellStyle::Rich(table.acquire(&rich, 1, None).0),
            };
            assert_eq!(style_attributes(style, &table), attrs);
            checked += 1;
        }
    }
    assert_eq!(checked, 5184 * 5);
}

#[test]
fn rich_table_grows_its_index_without_moving_ids() {
    let mut table = RichStyleTable::new();
    assert_eq!(table.index_slots(), STD_INDEX_SLOTS);
    let styles: Vec<RichStyle> = (0..2000).map(rich).collect();
    let ids: Vec<u32> = styles
        .iter()
        .map(|style| table.acquire(style, 1, None).0)
        .collect();
    assert_eq!(table.live(), 2000);
    // 2000 live ids need more than 2000 / 0.75 slots.
    assert_eq!(table.index_slots(), 4096);
    for (style, &id) in styles.iter().zip(&ids) {
        assert_eq!(table.get(id), Some(style));
        assert_eq!(table.acquire(style, 1, None).0, id);
    }
    let mut counts = vec![0; table.id_bound()];
    for &id in &ids {
        counts[id as usize] = 2;
    }
    table.check(&counts).unwrap();

    // Free every other style; the survivors stay findable through the
    // backward-shifted probe runs.
    for &id in ids.iter().step_by(2) {
        table.release(id);
        table.release(id);
        counts[id as usize] = 0;
    }
    table.check(&counts).unwrap();
    for (style, &id) in styles.iter().zip(&ids).skip(1).step_by(2) {
        assert_eq!(table.acquire(style, 1, None).0, id);
        table.release(id);
    }
    assert_eq!(table.live(), 1000);
    table.reset();
    assert!(table.is_clean());
}

#[test]
fn cached_rich_id_is_o1_for_repeats_and_never_trusted_stale() {
    let mut table = RichStyleTable::new();
    let a = rich(1);
    let (id, hint) = table.acquire(&a, 7, None);
    assert_eq!(table.hash_count(), 1);
    for _ in 0..100 {
        assert_eq!(table.acquire(&a, 7, Some(hint)), (id, hint));
    }
    assert_eq!(table.hash_count(), 1);
    assert_eq!(table.refs(id), 101);

    // A hint from another page incarnation, or for another style, is
    // checked and falls back to the hashed lookup.
    assert_eq!(table.acquire(&a, 8, Some(hint)).0, id);
    assert_eq!(table.hash_count(), 2);
    let (b_id, _) = table.acquire(&rich(2), 7, Some(hint));
    assert_ne!(b_id, id);
    assert_eq!(table.hash_count(), 3);

    // Freeing the entry bumps its generation, so the hint stays stale even
    // when the id is reused.
    for _ in 0..102 {
        table.release(id);
    }
    assert_eq!(table.get(id), None);
    let (reused, reused_hint) = table.acquire(&rich(3), 7, Some(hint));
    assert_eq!(reused, id);
    assert_ne!(reused_hint.generation, hint.generation);
    let (a_again, _) = table.acquire(&a, 7, Some(hint));
    assert_ne!(a_again, id);
    assert_eq!(table.get(a_again), Some(&a));
}

#[test]
fn sgr_cache_hook_makes_repeated_rich_prints_hash_free() {
    let mut page = Page::new(80, 4, 1);
    let row = page.grow(1).unwrap();
    let style = rich(5);
    let mut cache = None;
    for pass in 0..3 {
        for x in 0..80 {
            let spec = StyleSpec::Rich {
                style: &style,
                cache: &mut cache,
            };
            assert!(page.write(row, x, CellWrite::new(Glyph::Char('x'), spec), 2 + pass));
        }
    }
    assert_eq!(page.styles().hash_count(), 1);
    assert_eq!(page.styles().live(), 1);
    page.check_invariants().unwrap();
}

/// The T0 corpus pattern (`color-emoji-random.bin`): each glyph is a wide
/// emoji under a fresh random 256-colour foreground and background. The
/// inline encoding (ADR D2) keeps every one of the 65,536 fg x bg pairs out
/// of the rich table: no hashing, no entries and no index growth, first fill
/// or steady-state overwrite. Multi-scalar emoji take one 16-byte arena block
/// each, and overwrites reuse freed blocks, so the arena stays within one
/// block per glyph.
#[test]
fn t0_fresh_palette_fg_bg_per_wide_glyph_never_touches_the_style_table() {
    const EMOJI: [&str; 8] = [
        "😀",
        "😂",
        "🥰",
        "🤖",
        "🚀",
        "👍🏽",
        "👩\u{200d}💻",
        "🏳\u{fe0f}\u{200d}🌈",
    ];
    let cols = 120_u16;
    let mut page = Page::standard(cols, 1);
    let mut rng = 0x9E37_79B9_7F4A_7C15_u64;
    let mut seqno = 1;
    for pass in 0..3 {
        let mut written = 0;
        for row in 0..page.capacity() {
            if pass == 0 {
                assert_eq!(page.grow(seqno), Some(row));
            }
            for x in (0..usize::from(cols)).step_by(2) {
                rng = xorshift(rng);
                let fg = (rng & 0xFF) as u16;
                let bg = ((rng >> 8) & 0xFF) as u16;
                let text = EMOJI[(rng >> 16) as usize % EMOJI.len()];
                let style = InlineStyle::new(0, fg + 1, bg + 1);
                let mut write = CellWrite::new(Glyph::from_text(text), StyleSpec::Inline(style));
                write.wide = true;
                assert!(page.write(row, x, write, seqno));
                let attrs = page.cell_attributes(row, x);
                assert_eq!(attrs.foreground(), ColorAttribute::PaletteIndex(fg as u8));
                assert_eq!(attrs.background(), ColorAttribute::PaletteIndex(bg as u8));
                assert_eq!(page.glyph(row, x), Glyph::from_text(text));
                assert!(page.cell(row, x + 1).is_hidden());
                written += 1;
                seqno += 1;
            }
        }
        assert_eq!(written, 270 * 60);
        assert_eq!(page.styles().hash_count(), 0);
        assert_eq!(page.styles().live(), 0);
        assert_eq!(page.styles().index_slots(), STD_INDEX_SLOTS);
        assert!(page.graphemes().arena_bytes() <= 16 * 270 * 60);
        page.check_invariants().unwrap();
    }
}

#[test]
fn grapheme_arena_reuses_blocks_compacts_and_caps_length() {
    let mut arena = GraphemeArena::new();
    let texts: Vec<String> = (0..2000).map(|n| format!("e\u{301}{}", n)).collect();
    for (key, text) in texts.iter().enumerate() {
        arena.insert(key as u32, text);
    }
    arena.check().unwrap();
    let full = arena.arena_bytes();
    assert_eq!(full, 2000 * 16);

    // Freeing a block and storing a same-class grapheme reuses it.
    assert!(arena.remove(0));
    arena.insert(0, "a\u{300}");
    assert_eq!(arena.arena_bytes(), full);

    // Freeing most of the arena compacts it; survivors keep their text.
    for key in (0..2000).filter(|key| key % 10 != 0) {
        assert!(arena.remove(key));
    }
    assert!(arena.arena_bytes() <= STD_ARENA_BYTES);
    arena.check().unwrap();
    for key in (10..2000).step_by(10) {
        assert_eq!(arena.get(key), Some(texts[key as usize].as_str()));
    }

    arena.duplicate(10, 5000);
    assert_eq!(arena.get(5000), arena.get(10));
    let long = "x\u{301}".repeat(40);
    arena.insert(5001, &long);
    assert_eq!(arena.get(5001), Some(long.as_str()));
    let huge = "é".repeat(MAX_GRAPHEME_BYTES);
    arena.insert(5002, &huge);
    let kept = arena.get(5002).unwrap();
    assert!(kept.len() <= MAX_GRAPHEME_BYTES && huge.starts_with(kept));
    arena.check().unwrap();
    arena.reset();
    assert!(arena.is_clean());
}

/// Test fixtures: two hyperlinks and three images, one without a placement.
struct Fixture {
    links: Vec<Arc<Hyperlink>>,
    images: Vec<ImageCell>,
}

impl Fixture {
    fn new() -> Self {
        let image = |z, placement: Option<u32>| {
            ImageCell::with_z_index(
                TextureCoordinate::new_f32(0.0, 0.0),
                TextureCoordinate::new_f32(1.0, 1.0),
                Arc::new(ImageData::with_data(ImageDataType::placeholder())),
                z,
                0,
                0,
                0,
                0,
                placement.map(|_| 1),
                placement,
            )
        };
        Self {
            links: vec![
                Arc::new(Hyperlink::new("https://example.com/a")),
                Arc::new(Hyperlink::new_with_id("https://example.com/b", "b")),
            ],
            images: vec![image(0, Some(1)), image(-1, None), image(1, Some(3))],
        }
    }

    fn attrs(&self, spec: &AttrSpec) -> CellAttributes {
        let mut attrs = CellAttributes::default();
        attrs
            .set_intensity(intensity(spec.sgr[0]))
            .set_underline(underline(spec.sgr[1]))
            .set_blink(blink(spec.sgr[2]))
            .set_vertical_align(vertical_align(spec.sgr[3]))
            .set_italic(spec.flags[0])
            .set_reverse(spec.flags[1])
            .set_strikethrough(spec.flags[2])
            .set_invisible(spec.flags[3])
            .set_overline(spec.flags[4])
            .set_foreground(spec.fg.color())
            .set_background(spec.bg.color())
            .set_semantic_type(semantic(spec.semantic))
            .set_wrapped(spec.wrapped);
        if let Some(n) = spec.underline_color {
            attrs.set_underline_color(truecolor(u32::from(n)));
        }
        if let Some(link) = spec.link {
            attrs.set_hyperlink(Some(Arc::clone(&self.links[link])));
        }
        for &image in &spec.images {
            attrs.attach_image(Box::new(self.images[image].clone()));
        }
        attrs
    }
}

#[derive(Clone, Copy, Debug)]
enum ColorPick {
    Default,
    Palette(u8),
    True(u8),
}

impl ColorPick {
    fn color(self) -> ColorAttribute {
        match self {
            ColorPick::Default => ColorAttribute::Default,
            ColorPick::Palette(n) => ColorAttribute::PaletteIndex(n),
            ColorPick::True(n) => truecolor(u32::from(n)),
        }
    }
}

#[derive(Clone, Debug)]
struct AttrSpec {
    /// Intensity, underline, blink, vertical align.
    sgr: [u8; 4],
    /// Italic, reverse, strikethrough, invisible, overline.
    flags: [bool; 5],
    fg: ColorPick,
    bg: ColorPick,
    underline_color: Option<u8>,
    semantic: u8,
    wrapped: bool,
    link: Option<usize>,
    images: Vec<usize>,
}

/// Legacy cell texts and widths: narrow, blank, wide, and clusters
/// (narrow, wide, and one that starts with a space).
const GLYPHS: [(&str, usize); 9] = [
    ("a", 1),
    ("b", 1),
    (" ", 1),
    ("中", 2),
    ("😀", 2),
    ("e\u{301}", 1),
    ("👍🏽", 2),
    (" \u{301}", 1),
    ("👩\u{200d}💻", 2),
];

const COLS: u16 = 8;
const ROWS: u32 = 3;

#[derive(Clone, Debug)]
enum Op {
    Write {
        row: usize,
        x: usize,
        glyph: usize,
        attrs: AttrSpec,
        clear: bool,
    },
    Insert {
        row: usize,
        x: usize,
        margin: usize,
    },
    Erase {
        row: usize,
        x: usize,
        margin: usize,
        blank: AttrSpec,
    },
    Resize {
        row: usize,
        width: usize,
    },
    Copy {
        src: usize,
        dst: usize,
        x: usize,
        n: usize,
    },
    Clear {
        row: usize,
    },
    RewriteHidden {
        row: usize,
    },
    Reset,
}

fn color_pick() -> impl Strategy<Value = ColorPick> {
    prop_oneof![
        3 => Just(ColorPick::Default),
        4 => any::<u8>().prop_map(ColorPick::Palette),
        1 => (0..3_u8).prop_map(ColorPick::True),
    ]
}

fn attr_spec() -> impl Strategy<Value = AttrSpec> {
    (
        (
            any::<[u8; 4]>(),
            any::<[bool; 5]>(),
            color_pick(),
            color_pick(),
        ),
        (
            prop::option::weighted(0.15, 0..3_u8),
            0..3_u8,
            any::<bool>(),
            prop::option::weighted(0.2, 0..2_usize),
            prop::collection::vec(0..3_usize, 0..3),
        ),
    )
        .prop_map(
            |((sgr, flags, fg, bg), (underline_color, semantic, wrapped, link, images))| AttrSpec {
                sgr,
                flags,
                fg,
                bg,
                underline_color,
                semantic,
                wrapped,
                link,
                images,
            },
        )
}

fn op() -> impl Strategy<Value = Op> {
    let row = 0..ROWS as usize;
    let col = 0..usize::from(COLS);
    let width = 0..=usize::from(COLS);
    let margin = 1..=usize::from(COLS);
    prop_oneof![
        8 => (row.clone(), col.clone(), 0..GLYPHS.len(), attr_spec(), prop::bool::weighted(0.2))
            .prop_map(|(row, x, glyph, attrs, clear)| Op::Write { row, x, glyph, attrs, clear }),
        2 => (row.clone(), col.clone(), margin.clone())
            .prop_map(|(row, x, margin)| Op::Insert { row, x, margin }),
        2 => (row.clone(), col.clone(), margin, attr_spec())
            .prop_map(|(row, x, margin, blank)| Op::Erase { row, x, margin, blank }),
        1 => (row.clone(), width.clone()).prop_map(|(row, width)| Op::Resize { row, width }),
        1 => (row.clone(), row.clone(), col, width)
            .prop_map(|(src, dst, x, n)| Op::Copy { src, dst, x, n }),
        1 => row.clone().prop_map(|row| Op::Clear { row }),
        1 => row.prop_map(|row| Op::RewriteHidden { row }),
        1 => Just(Op::Reset),
    ]
}

/// A page and the legacy lines it must reproduce, driven op by op.
struct Harness {
    page: Page,
    lines: Vec<Line>,
    fixture: Fixture,
    seqno: usize,
    serial: u64,
}

impl Harness {
    fn new() -> Self {
        let mut harness = Self {
            page: Page::new(COLS, ROWS, 1),
            lines: Vec::new(),
            fixture: Fixture::new(),
            seqno: 1,
            serial: 1,
        };
        harness.fill_rows();
        harness
    }

    fn fill_rows(&mut self) {
        self.lines = (0..ROWS)
            .map(|_| Line::from_cells(vec![], self.seqno))
            .collect();
        for row in 0..ROWS {
            assert_eq!(self.page.grow(self.seqno), Some(row));
        }
    }

    fn apply(&mut self, op: &Op) -> Result<(), String> {
        self.seqno += 1;
        let seqno = self.seqno;
        let cols = usize::from(COLS);
        match op {
            Op::Write {
                row,
                x,
                glyph,
                attrs,
                clear,
            } => {
                let (text, width) = GLYPHS[*glyph];
                let cell = Cell::new_grapheme_with_width(text, width, self.fixture.attrs(attrs));
                if *clear {
                    self.lines[*row].set_cell_clearing_image_placements(*x, cell.clone(), seqno);
                } else {
                    self.lines[*row].set_cell(*x, cell.clone(), seqno);
                }
                assert!(self
                    .page
                    .write_legacy(*row as u32, *x, &cell, *clear, seqno));
            }
            Op::Insert { row, x, margin } => {
                let line = &mut self.lines[*row];
                line.insert_cell(*x, Cell::default(), *margin, seqno);
                if line.len() > cols {
                    line.resize(cols, seqno);
                }
                self.page
                    .insert_blank(*row as u32, *x, *margin, cols, seqno);
            }
            Op::Erase {
                row,
                x,
                margin,
                blank,
            } => {
                let attrs = self.fixture.attrs(blank);
                self.lines[*row].erase_cell_with_margin(*x, *margin, seqno, attrs.clone());
                erase_with_attrs(&mut self.page, *row as u32, *x, *margin, &attrs, seqno);
            }
            Op::Resize { row, width } => {
                self.lines[*row].resize(*width, seqno);
                self.page.resize_row(*row as u32, *width, seqno);
            }
            Op::Copy { src, dst, x, n } => {
                let cells: Vec<Cell> = self.lines[*src]
                    .cells_mut()
                    .iter()
                    .skip(*x)
                    .take(*n)
                    .cloned()
                    .collect();
                let count = cells.len();
                if src != dst {
                    let line = &mut self.lines[*dst];
                    if line.len() < x + count {
                        line.resize(x + count, seqno);
                    }
                    for (offset, cell) in cells.into_iter().enumerate() {
                        line.cells_mut()[x + offset] = cell;
                    }
                }
                let copied = self
                    .page
                    .copy_cells(*src as u32, *dst as u32, *x, *n, seqno);
                if copied != count {
                    return Err(format!("copied {} cells, legacy {}", copied, count));
                }
            }
            Op::Clear { row } => {
                self.lines[*row].resize(0, seqno);
                self.page.clear_row(*row as u32, seqno);
            }
            Op::RewriteHidden { row } => {
                // What legacy's clustered storage rebuilds (ADR Q3): each
                // hidden cell becomes a blank with its head's attributes.
                let cells = self.lines[*row].cells_mut();
                let mut skip = 0;
                for x in 0..cells.len() {
                    if skip > 0 {
                        skip -= 1;
                        cells[x] = Cell::blank_with_attrs(cells[x - 1].attrs().clone());
                    } else {
                        skip = cells[x].width().saturating_sub(1);
                    }
                }
                self.page.rewrite_hidden_cells(*row as u32);
            }
            Op::Reset => {
                self.serial += 1;
                self.page.reset(self.serial);
                if !self.page.is_clean() {
                    return Err("reset left the page unclean".to_string());
                }
                self.fill_rows();
            }
        }
        self.page.check_invariants()?;
        self.compare()
    }

    /// Every stored cell, hidden ones included, against legacy.
    fn compare(&mut self) -> Result<(), String> {
        for (row, line) in self.lines.iter_mut().enumerate() {
            let row = row as u32;
            if self.page.row_len(row) != line.len() {
                return Err(format!(
                    "row {} len {} but legacy {}",
                    row,
                    self.page.row_len(row),
                    line.len()
                ));
            }
            for (x, legacy) in line.cells_mut().iter().enumerate() {
                let ours = self.page.legacy_cell(row, x);
                let mut our_attrs = ours.attrs().clone();
                our_attrs.clear_images();
                let mut legacy_attrs = legacy.attrs().clone();
                legacy_attrs.clear_images();
                let our_images: Vec<ImageCell> = self
                    .page
                    .cell_images(row, x)
                    .map(|images| images.iter().map(|image| (**image).clone()).collect())
                    .unwrap_or_default();
                let legacy_images = legacy.attrs().images().unwrap_or_default();
                if ours.str() != legacy.str()
                    || ours.width() != legacy.width()
                    || our_attrs != legacy_attrs
                    || our_images != legacy_images
                {
                    return Err(format!(
                        "row {} cell {}: page {:?} images {:?}, legacy {:?}",
                        row, x, ours, our_images, legacy
                    ));
                }
            }
        }
        Ok(())
    }
}

fn erase_with_attrs(
    page: &mut Page,
    row: u32,
    x: usize,
    margin: usize,
    attrs: &CellAttributes,
    seqno: usize,
) {
    let images: Vec<Box<ImageCell>> = attrs
        .images()
        .map(|images| images.into_iter().map(Box::new).collect())
        .unwrap_or_default();
    let class = classify(attrs);
    let mut cache = None;
    let style = match &class {
        StyleClass::Inline(inline) => StyleSpec::Inline(*inline),
        StyleClass::Rich(rich) => StyleSpec::Rich {
            style: rich,
            cache: &mut cache,
        },
    };
    let mut blank = CellWrite::new(Glyph::Blank, style);
    blank.semantic = attrs.semantic_type();
    blank.wrapped = attrs.wrapped();
    blank.hyperlink = attrs.hyperlink();
    blank.images = &images;
    page.erase_cell_with_margin(row, x, margin, blank, seqno);
}

fn plain() -> AttrSpec {
    AttrSpec {
        sgr: [0; 4],
        flags: [false; 5],
        fg: ColorPick::Default,
        bg: ColorPick::Default,
        underline_color: None,
        semantic: 0,
        wrapped: false,
        link: None,
        images: vec![],
    }
}

fn write(row: usize, x: usize, glyph: usize, attrs: AttrSpec) -> Op {
    Op::Write {
        row,
        x,
        glyph,
        attrs,
        clear: false,
    }
}

/// Hand-picked edges: the overhang cell, overwriting a spacer, ICH and DCH
/// across wide cells, placement carry-over, and band copies of rich,
/// linked, imaged and grapheme cells.
#[test]
fn edge_sequences_match_legacy() {
    let red_link = AttrSpec {
        fg: ColorPick::True(1),
        link: Some(0),
        images: vec![0, 1],
        ..plain()
    };
    let sequences: Vec<Vec<Op>> = vec![
        // A wide glyph at the last column overhangs; DCH pulls the overhang
        // into view and ICH pushes it out again.
        vec![
            write(0, 7, 3, red_link.clone()),
            Op::Erase {
                row: 0,
                x: 0,
                margin: 8,
                blank: plain(),
            },
            Op::Insert {
                row: 0,
                x: 0,
                margin: 8,
            },
        ],
        // Overwriting a spacer blanks its head; the head's placement image
        // carries into the new cell.
        vec![
            write(1, 2, 6, red_link.clone()),
            write(1, 3, 0, plain()),
            write(1, 2, 1, plain()),
        ],
        // ICH inside a wide pair leaves a hidden cell with its own style.
        vec![
            write(2, 0, 4, red_link.clone()),
            write(2, 2, 8, plain()),
            Op::Insert {
                row: 2,
                x: 1,
                margin: 8,
            },
            Op::RewriteHidden { row: 2 },
            Op::Copy {
                src: 2,
                dst: 0,
                x: 0,
                n: 8,
            },
            Op::Resize { row: 0, width: 1 },
            write(0, 4, 5, plain()),
        ],
        // A write far past the end pads, and the padding cell after a
        // truncated wide head is hidden.
        vec![
            write(1, 0, 3, plain()),
            Op::Resize { row: 1, width: 1 },
            write(1, 5, 0, plain()),
            Op::Reset,
            write(1, 6, 7, red_link),
        ],
    ];
    for ops in sequences {
        let mut harness = Harness::new();
        for (step, op) in ops.iter().enumerate() {
            if let Err(err) = harness.apply(op) {
                panic!("step {} {:?}: {}", step, op, err);
            }
        }
    }
}

/// Text, width and attributes; `Cell` equality also compares how the text
/// is stored, which legacy builds differently for the same blank.
fn assert_same_cell(ours: &Cell, legacy: &Cell) {
    assert!(
        ours.str() == legacy.str()
            && ours.width() == legacy.width()
            && ours.attrs() == legacy.attrs(),
        "page {:?} legacy {:?}",
        ours,
        legacy
    );
}

/// ADR Q3 against the real legacy path: compressing to clustered storage
/// and re-materializing rebuilds a hidden cell from its head, which is what
/// `rewrite_hidden_cells` does.
#[test]
fn hidden_cell_rewrite_matches_legacy_recompression() {
    let mut red = CellAttributes::default();
    red.set_foreground(ColorAttribute::PaletteIndex(1))
        .set_intensity(Intensity::Bold);
    let head = Cell::new_grapheme_with_width("中", 2, red.clone());
    let tail = Cell::new_grapheme_with_width("x", 1, CellAttributes::default());

    let mut line = Line::from_cells(vec![], 1);
    line.set_cell(0, head.clone(), 1);
    line.set_cell(2, tail.clone(), 1);
    line.insert_cell(1, Cell::default(), 8, 2);
    let mut page = Page::new(8, 1, 1);
    let row = page.grow(1).unwrap();
    page.write_legacy(row, 0, &head, false, 1);
    page.write_legacy(row, 2, &tail, false, 1);
    page.insert_blank(row, 1, 8, 8, 2);
    assert_same_cell(&page.legacy_cell(row, 1), &line.cells_mut()[1]);
    assert_eq!(line.cells_mut()[1].attrs(), &CellAttributes::default());

    line.compress_for_scrollback();
    let rebuilt = line.cells_mut().to_vec();
    assert_eq!(
        rebuilt[1].attrs(),
        &red,
        "legacy rebuilt the hidden cell from its head"
    );
    assert_eq!(page.rewrite_hidden_cells(row), 1);
    assert_eq!(page.row_len(row), rebuilt.len());
    for (x, cell) in rebuilt.iter().enumerate() {
        assert_same_cell(&page.legacy_cell(row, x), cell);
    }
    page.check_invariants().unwrap();
}

#[test]
fn reset_returns_a_used_page_to_a_clean_state() {
    let fixture = Fixture::new();
    let mut page = Page::standard(120, 1);
    let mut cache = None;
    let style = rich(4);
    let images: Vec<Box<ImageCell>> = vec![Box::new(fixture.images[0].clone())];
    for row in 0..40 {
        page.grow(row as usize + 1).unwrap();
        for x in (0..118).step_by(3) {
            let mut write = CellWrite::new(
                Glyph::from_text("👩\u{200d}💻"),
                StyleSpec::Rich {
                    style: &style,
                    cache: &mut cache,
                },
            );
            write.wide = true;
            write.hyperlink = Some(&fixture.links[x % 2]);
            write.images = &images;
            write.semantic = SemanticType::Prompt;
            page.write(row, x, write, row as usize + 2);
        }
    }
    page.check_invariants().unwrap();
    assert!(!page.is_clean());
    let old_serial = page.serial();
    page.reset(old_serial + 1);
    assert!(page.is_clean());
    assert_eq!(page.serial(), old_serial + 1);
    page.check_invariants().unwrap();

    // The pen's cached id names the old incarnation and is not trusted.
    let row = page.grow(1).unwrap();
    let spec = StyleSpec::Rich {
        style: &style,
        cache: &mut cache,
    };
    page.write(row, 0, CellWrite::new(Glyph::Char('y'), spec), 2);
    assert_eq!(page.styles().hash_count(), 1);
    assert_eq!(cache.map(|cached| cached.page_serial), Some(old_serial + 1));
    page.check_invariants().unwrap();
}

#[test]
fn page_seqno_maximum_tracks_rows() {
    let mut page = Page::new(10, 4, 1);
    let a = page.grow(5).unwrap();
    let b = page.grow(0).unwrap();
    assert_eq!(page.max_seqno(), 0, "a row at 0 is always changed");
    page.touch_row(a, 9);
    assert_eq!(page.max_seqno(), 0);
    page.touch_row(b, 7);
    assert_eq!(page.max_seqno(), 9);
    page.write(
        b,
        0,
        CellWrite::new(Glyph::Char('z'), StyleSpec::Inline(InlineStyle::DEFAULT)),
        12,
    );
    assert_eq!((page.row_seqno(b), page.max_seqno()), (12, 12));
    assert!(page.take_dirty(b));
    assert!(!page.take_dirty(b));
    page.check_invariants().unwrap();
}

proptest! {
    /// Random op sequences on a small page: after every op the page
    /// invariants hold (refcounts equal cell counts, no dangling or leaked
    /// ids, arena and side maps consistent, reset clean) and every stored
    /// cell equals the legacy `Line`'s.
    #[test]
    fn op_sequences_keep_invariants_and_match_legacy(ops in prop::collection::vec(op(), 1..64)) {
        let mut harness = Harness::new();
        for (step, op) in ops.iter().enumerate() {
            harness
                .apply(op)
                .map_err(|err| TestCaseError::fail(format!("step {} {:?}: {}", step, op, err)))?;
        }
        let serial = harness.serial + 1;
        harness.page.reset(serial);
        prop_assert!(harness.page.is_clean());
    }

    /// The rich table alone, under acquire and release with and without
    /// hints, past index growth and through backward-shift deletes.
    #[test]
    fn rich_table_refcounts_survive_random_acquire_release(
        ops in prop::collection::vec((0..1000_u32, prop::bool::weighted(0.65), any::<bool>()), 1..800)
    ) {
        let mut table = RichStyleTable::new();
        let mut held: Vec<u32> = Vec::new();
        let mut hints = vec![None; 1000];
        for (n, acquire, use_hint) in ops {
            if acquire || held.is_empty() {
                let style = rich(n);
                let hint = if use_hint { hints[n as usize] } else { None };
                let (id, cached) = table.acquire(&style, 1, hint);
                prop_assert_eq!(table.get(id), Some(&style));
                hints[n as usize] = Some(cached);
                held.push(id);
            } else {
                let id = held.swap_remove(n as usize % held.len());
                table.release(id);
            }
            let mut counts = vec![0; table.id_bound()];
            for &id in &held {
                counts[id as usize] += 1;
            }
            table.check(&counts).map_err(TestCaseError::fail)?;
        }
        for id in held {
            table.release(id);
        }
        prop_assert_eq!(table.live(), 0);
        table.reset();
        prop_assert!(table.is_clean());
    }
}
