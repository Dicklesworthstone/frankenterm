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

/// `(relative_path, global_ident)` pairs to skip. Paths are rooted
/// at the scanned `src` root (e.g. `crates/frankenterm-gui/src`) and
/// use forward slashes.
///
/// Currently EMPTY: the post-fix tree has zero process-global caches
/// that reach a GPU-handle leaf. If this list grows, treat each entry
/// as a latent leak waiting to be reintroduced and prefer a structural
/// fix.
pub const ALLOWED_GLOBALS: &[(&str, &str)] = &[];
