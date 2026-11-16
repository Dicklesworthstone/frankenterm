// Fixture: the POST-FIX shape — a process-global cache that holds ONLY
// position / metric POD. It reaches NO GPU-handle leaf, so the lint
// MUST report it clean. This mirrors `ShapedInfoTemplate`: cache the
// atlas-INVARIANT shaping slice, never the glyph / Sprite / texture.
//
// Type names are fixture-unique (`Good*` prefix) so the shared union
// type graph never mixes these defs with the bad fixture's.

use std::cell::RefCell;
use std::collections::HashMap;

thread_local! {
    static GOOD_SHAPED_RUN_INTERNER: RefCell<GoodInterner> = RefCell::new(GoodInterner::default());
}

#[derive(Default)]
struct GoodInterner {
    runs: HashMap<u64, Vec<GoodRun>>,
}

struct GoodRun {
    // Only invariant templates — no Rc<CachedGlyph>, no Sprite.
    templates: Vec<GoodShapedInfoTemplate>,
}

struct GoodShapedInfoTemplate {
    pos: GoodGlyphPosition,
    block_key: Option<u32>,
}

struct GoodGlyphPosition {
    glyph_idx: u32,
    num_cells: u8,
    x_offset: f32,
    bearing_x: f32,
    bitmap_pixel_width: u32,
}

// A second clean global for good measure: pure scalar data.
static GOOD_GLYPH_METRICS: GoodGlyphPosition = GoodGlyphPosition {
    glyph_idx: 0,
    num_cells: 1,
    x_offset: 0.0,
    bearing_x: 0.0,
    bitmap_pixel_width: 0,
};
