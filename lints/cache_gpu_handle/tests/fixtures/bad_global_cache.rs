// Fixture: reproduces the PRE-FIX shape of the glyph-run interner.
//
// A `thread_local!` process-global cache holds a struct that
// transitively reaches a (fake) `CachedGlyph` / `Sprite` — exactly the
// atlas-pinning GPU-memory leak class. The lint MUST flag the global.
//
//   BAD_SHAPED_RUN_INTERNER : RefCell<BadInterner>
//     -> BadInterner -> BadRun -> BadInfo -> CachedGlyph -> Sprite (FORBIDDEN)
//
// NOTE: type names are fixture-unique (`Bad*` prefix). The analyzer
// builds ONE type graph over the union of all scanned files, so two
// fixtures sharing a struct name would cross-contaminate each other's
// graphs (which is correct behavior — the real codebase has unique type
// names). The `CachedGlyph` / `Sprite` leaf idents are intentionally the
// real forbidden names.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

thread_local! {
    static BAD_SHAPED_RUN_INTERNER: RefCell<BadInterner> = RefCell::new(BadInterner::default());
}

#[derive(Default)]
struct BadInterner {
    runs: HashMap<u64, Vec<BadRun>>,
}

struct BadRun {
    shaped: Vec<BadInfo>,
}

struct BadInfo {
    // The leak vector: a strong ref to a glyph that owns a GPU texture.
    glyph: Rc<CachedGlyph>,
}

struct CachedGlyph {
    // Transitively owns the ~tens-of-MB GPU atlas texture.
    texture: Option<Sprite>,
}

struct Sprite {
    // Stand-in for `Rc<dyn Texture2d>` — the GPU resource handle.
    gpu: u32,
}
