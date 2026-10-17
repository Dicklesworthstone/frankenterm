//! Allow-list for the cache-gpu-handle lint.
//!
//! A single carve-out kind, deliberately narrow.
//!
//! [`ALLOWED_GLOBALS`] — `(relative_path, global_ident)` pairs for
//! process-global containers whose root type *does* transitively
//! reach a forbidden GPU-handle leaf, but which are nonetheless
//! known-safe (e.g. the global itself is the legitimate owner whose
//! lifetime is bounded by an explicit clear, or it holds a `Weak`
//! that the type-graph walk can't yet distinguish).
//!
//! Aim for **zero entries**. The whole point of the lint is that the
//! leak class is *unrepresentable*; every allow-list entry is a hole.
//! Each entry MUST carry an inline comment justifying why the global
//! cannot pin a GPU resource for longer than one atlas generation,
//! and SHOULD be paired with a runtime regression test (mirroring
//! `shapecache.rs::interner_does_not_pin_glyphs_*`).
//!
//! # Adding a new exemption
//!
//! 1. Confirm the global genuinely cannot outlive a single atlas
//!    generation (it stores a `Weak`, or it is cleared on every
//!    atlas recreation, or the "leaf" it reaches is a false alias).
//! 2. Add a comment here explaining why.
//! 3. Prefer fixing the type instead — store the atlas-INVARIANT
//!    slice (positions / metrics) and re-attach the live glyph on
//!    hit, as `ShapedInfoTemplate` does. That makes the class
//!    unrepresentable rather than merely allow-listed.
//!
//! # Known limitation: the type graph is keyed by *bare* type name
//!
//! The reachability graph unions edges by unqualified type name, so
//! two distinct types that happen to share a name in different modules
//! (the scanned tree has several: `Inner`, `Item`, `Window`, …) become
//! one node whose edges are the *union* of all of them. This can only
//! ever produce a FALSE POSITIVE (it adds edges, never removes them —
//! so the leak class can never be silently *missed*): a benign global
//! routed through a shared name could be flagged because an unrelated
//! same-named type reaches a GPU leaf. It is not tripped today. If it
//! ever is, confirm the *specific* type the global actually references
//! is benign, then add an `ALLOWED_GLOBALS` entry (with that
//! justification) rather than reading it as a real leak. Qualifying
//! nodes by module path would remove the limitation but is a larger
//! refactor deliberately deferred while the union stays false-positive-
//! only and untripped.

/// `(relative_path, global_ident)` pairs to skip. Paths are rooted
/// at the scanned `src` root (e.g. `crates/frankenterm-gui/src`) and
/// use forward slashes.
///
/// Currently EMPTY: the post-fix tree has zero process-global caches
/// that reach a GPU-handle leaf. If this list grows, treat each entry
/// as a latent leak waiting to be reintroduced and prefer a structural
/// fix.
pub const ALLOWED_GLOBALS: &[(&str, &str)] = &[];

/// A per-window cache field that legitimately holds GPU-backed glyphs
/// between atlas rebuilds because every rebuild releases them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheFieldAllowEntry {
    /// File holding the struct, relative to the scanned src root.
    pub path: &'static str,
    /// The struct that declares the field.
    pub owner: &'static str,
    /// The field name.
    pub field: &'static str,
    /// The code that releases the field's GPU-backed handles on every
    /// atlas rebuild.
    pub released_by: &'static str,
    /// The runtime test proving the release frees the old atlas.
    pub proven_by: &'static str,
}

/// Per-window cache fields fenced by an atlas-rebuild release (rule 6 of
/// the crate docs). Each entry names its release site and the runtime test
/// that proves the old atlas texture is freed. An entry that stops matching
/// a GPU-reaching field is reported as stale and must be removed.
pub const ALLOWED_CACHE_FIELDS: &[CacheFieldAllowEntry] = &[
    // `TermWindow.shape_cache` is no longer listed: since ft-yccm0.4.3.4 its
    // entries hold only HarfBuzz output (`CachedShape` of `GlyphInfo`s), and
    // the resolved sprites live in `GlyphCache.shape_bindings`, beside the
    // atlas they point into. This lint now proves the shape cache cannot
    // reach a GPU handle, instead of fencing it.
    //
    // Line shapes carry `Rc<Vec<ShapedInfo>>`; the whole cache is cleared for
    // every rebuild at or above `CacheRebuild::ColoredLines`, which includes
    // atlas rebuilds.
    CacheFieldAllowEntry {
        path: "termwindow/mod.rs",
        owner: "TermWindow",
        field: "line_to_ele_shape_cache",
        released_by: "termwindow/resize.rs: clear_generation_keyed_line_shape_cache",
        proven_by: "termwindow::resize::tests::\
                    thousand_atlas_rebuilds_keep_live_atlas_bytes_within_twice_the_atlas_size",
    },
];
