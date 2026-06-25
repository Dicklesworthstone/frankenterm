# GPU-cache lifetime invariant

> **The rule:** A process-global cache (`thread_local!`, `static`, `lazy_static!`,
> `LazyLock`/`OnceLock`) in the GUI must **never own a GPU-resource handle.** Cache
> the *invariant* (the CPU result), reference the *mutable* GPU resource weakly, or
> re-resolve it from its real owner each use.

This is the standing rule that prevents the "cache pins an atlas generation" class
of GPU-memory leak. It is enforced statically (CI lint) and proven at test time.

---

## Why — the case study that motivated it

The macOS GUI degraded progressively over hours ("starts fast, gets laggy, a full
restart fixes it"). A live `footprint` of the running process showed ~1.5 GB held
almost entirely in **GPU surfaces** — 691 MB of IOSurface across **14 ~49 MB
textures**, plus 439 MB of CoreMedia capture-staging for them — while the entire
Rust heap was ~28 MB. Fourteen retained ~49 MB textures is the fingerprint of an
**atlas-generation leak**.

Root cause: the thread-local glyph-run shaping cache (`SHAPED_RUN_INTERNER` in
`crates/frankenterm-gui/src/shapecache.rs`) cached each shaped run as
`Vec<ShapedInfo>`, and `ShapedInfo` bundled `glyph: Rc<CachedGlyph>`. A
`CachedGlyph` transitively **owns a GPU atlas texture**:

```
CachedGlyph.texture: Option<Sprite>
  -> Sprite.texture: Rc<dyn Texture2d>
    -> WebGpuTexture -> wgpu::Texture   (~tens of MB, IOSurface-backed on Metal)
```

When the glyph atlas fills and is recreated, the recreate path cleared the *other*
shape caches but never the interner. The interner self-evicts only at 8192 entries,
so under endless varied colored text (a terminal swarm) it stayed full and kept
`Rc<CachedGlyph>` runs pointing into **prior** atlas generations alive — ~14 of
them. A restart freed every atlas; the lag returned as they re-accumulated.

The cache was conflating two things with **different lifetimes**: the shaping
result (text+style → glyph indices + positions; pure CPU work, invariant across
atlas recreations) and GPU residency (`Sprite`/atlas slot; rebuilt on every
recreation).

---

## The fix — decompose by lifetime (alien-artifact, not alien-graveyard)

The shaping cache now caches **only the atlas-invariant data** (`GlyphPosition` +
`BlockKey`, as `ShapedInfoTemplate`) and **re-attaches the caller's current glyphs**
on every hit (`InternedShapedRun::rebuild`). It holds zero `Rc<CachedGlyph>`, so it
*cannot* extend any atlas's lifetime. This is:

- **leak-free by construction** — a provable invariant ("the cache holds no GPU
  handle ⇒ it cannot pin an atlas"), not a fragile "remember to clear at N sites";
- **more correct** — it always renders from the live atlas, never a stale one;
- **faster** than clearing on recreate — the shaping cache *survives* recreation
  instead of being thrown away.

The deliberate non-goal: a heavyweight runtime surface/texture pool with explicit
reclamation ("idea 2"). After this fix an interned entry is tiny POD (positions),
never a GPU texture, so a runtime pool would be pure overhead. The "no silent
unbounded growth" guarantee is delivered more robustly — and at **zero runtime
cost** — by the static lint + the behavior-proof test below. Reaching for
generational arenas / epoch reclamation / hazard pointers for a single-threaded
`thread_local` cache would be the alien-graveyard anti-pattern: clever, wrong tool,
more bug surface, negative EV.

---

## Enforcement (so it cannot silently regress)

1. **Static lint — make the bug class unrepresentable.**
   `lints/cache_gpu_handle/` is a `syn`-based source analyzer (same stable-Rust
   pattern as `lints/cx_propagation`). It builds a type-reachability graph and
   fails CI if any process-global container's type transitively reaches a forbidden
   GPU-handle leaf (`Sprite`, `CachedGlyph`, `Texture2d`, `WebGpuTexture`,
   `wgpu::Texture`/`TextureView`). It correctly spares legitimate ownership (the
   real `GlyphCache`/atlas owner is a *field* of `TermWindow`/`RenderState`, never a
   process-global). Run:
   `cargo run -q --release -p cache_gpu_handle_lint -- crates/frankenterm-gui/src frankenterm/window/src`
   (wired into `.github/workflows/finish-line-guards.yml`).

2. **Behavior proof + leak guard (tests).** In `shapecache.rs`:
   - `interner_does_not_pin_glyphs_atlas_generation_leak_guard` — asserts
     `Rc::strong_count` stays 1 after interning (the interner adds no strong ref).
   - `churn_across_atlas_recreations_reclaims_every_old_generation` — drives 64
     simulated atlas recreations and asserts **zero** past-generation glyphs survive
     (the leak would keep all of them alive).
   - `interned_run_matches_fresh_shape_for_same_font_and_attrs` — the cache still
     produces output identical to fresh shaping (the decoupling is isomorphic).

3. **Runtime signal.** `gui.shaped_run_interner.clear_evictions` (emitted only on
   the rare 8192-entry cap, never per frame) surfaces cache thrashing in `ft`'s
   stats — the "no silent unbounded growth" observability hook.

---

## The standing rule for future caches (idea 3: weak-ref / re-resolve)

When you add a cache that *needs* to reference a resource it does not own:

- **Prefer**: cache only the invariant (the CPU result) and re-resolve the resource
  from its real owner on use — what the shaping interner now does.
- **Otherwise**: hold a `Weak<_>` to the resource and treat a failed upgrade as a
  cache miss. Never an owning `Rc`/`Arc` to a `Sprite`/texture/atlas/render-target
  in a long-lived/global cache.
- The `cache_gpu_handle` lint will reject a violation at CI time; do not add an
  allow-list entry to silence it without decomposing the lifetime first.
