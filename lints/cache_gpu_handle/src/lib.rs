//! Custom lint: forbid any **process-global** long-lived container from
//! transitively owning a **GPU-resource handle**.
//!
//! **Class:** glyph-run-interner atlas-pinning GPU-memory leak.
//! **Doc:** see `crates/frankenterm-gui/src/shapecache.rs` doc comments.
//!
//! # The bug this makes unrepresentable
//!
//! A `thread_local!` glyph-run interner in `shapecache.rs` cached
//! `Rc<CachedGlyph>`. A `CachedGlyph` transitively owns a ~tens-of-MB
//! GPU atlas texture:
//!
//! ```text
//! CachedGlyph.texture: Option<Sprite>
//!     -> Sprite.texture: Rc<dyn Texture2d>   (the GPU atlas)
//! ```
//!
//! Because the interner is a *process-global* (`thread_local!`) cache,
//! every old `Rc<CachedGlyph>` it retained pinned the atlas texture of
//! a stale atlas generation. Atlas recreation (resize / DPI change /
//! grow) then leaked the previous generation — 691 MB of IOSurface was
//! observed in the field. The fix caches the atlas-INVARIANT shaping
//! slice (`GlyphPosition` + `BlockKey`, no `Rc`) and re-attaches the
//! *current* glyph on each cache hit, so the global cache can never
//! own a GPU resource.
//!
//! # Why a syn-based analyzer rather than a cargo-dylint plugin
//!
//! Mirrors the sibling `cx_propagation_lint` crate's rationale: a
//! `LateLintPass` plugin requires riding unstable rustc internals on a
//! pinned nightly. The rule here is purely *syntactic* — a reachability
//! query over a type graph built from `struct`/`enum`/type-alias defs —
//! so `syn` (stable Rust) expresses it exactly, with a real AST instead
//! of regexes, and runs as an ordinary `cargo run` / CI step.
//!
//! # The rule
//!
//! 1. **Forbidden GPU-handle leaf set.** The leaf types that *are* (or
//!    directly own) a GPU resource: [`FORBIDDEN_LEAVES`]. Matching is on
//!    the last path segment, so `crate::bitmaps::Sprite`, `wgpu::Texture`,
//!    and a bare `Sprite` all match.
//!
//! 2. **Type graph.** From every `struct` / `enum` / `type` alias in the
//!    scanned dirs, build `type_name -> {referenced type idents}`. Every
//!    path-segment ident appearing anywhere in a field/variant type is an
//!    edge — including inside generics (`Vec<T>`, `Rc<T>`, `Option<T>`,
//!    `RefCell<T>`, `AHashMap<K, V>`, tuples, arrays, references, boxes).
//!    This deliberately over-approximates ownership (a `Weak<T>` edge is
//!    treated like an `Rc<T>` edge); the allow-list is the escape hatch
//!    for the rare false positive.
//!
//! 3. **Process-global roots.** Find every long-lived process-global
//!    container and extract its *root type* `T`:
//!    - `thread_local! { static X: T = ...; }`
//!    - top-level `static X: T = ...;` / `static ref X: T` (lazy_static)
//!    - `lazy_static! { static ref X: T = ...; }`
//!    - `static X: LazyLock<T> = ...;` / `OnceLock<T>` (root is the inner
//!      `T`, since `LazyLock`/`OnceLock` are just the global cell).
//!
//! 4. **Reachability.** For each global root type, BFS the type graph.
//!    If it reaches ANY forbidden leaf, emit a finding with the witness
//!    path through the graph, e.g.:
//!    `SHAPED_RUN_INTERNER : RefCell<ShapedRunInterner> -> ShapedRunInterner
//!     -> InternedShapedRun -> ShapedInfo -> CachedGlyph (FORBIDDEN)`.
//!
//! # What the rule deliberately SPARES
//!
//! The legitimate atlas owner (`GlyphCache` / render state) is a *field*
//! of `TermWindow` / `RenderState`, never a process-global. It is never a
//! root, so it is never checked. Only process-global caches are roots.
//! This is exactly why the post-fix interner is clean: it stores
//! `ShapedInfoTemplate` (no `Rc<CachedGlyph>`), so the global root no
//! longer reaches a forbidden leaf.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use syn::visit::Visit;
use syn::{Fields, Item, ItemEnum, ItemStruct, ItemType, Macro, Type};

pub mod allow_list;

/// The forbidden GPU-handle leaf types. Matching is on the *last*
/// path segment, so any import path / alias resolves.
///
/// - `Sprite` — `frankenterm/window/src/bitmaps/atlas.rs`; owns
///   `Rc<dyn Texture2d>` (the GPU atlas texture).
/// - `CachedGlyph` — `crates/frankenterm-gui/src/glyphcache.rs`; owns
///   `Option<Sprite>`.
/// - `Texture2d` — `frankenterm/window/src/bitmaps/mod.rs`; the
///   `dyn Texture2d` trait object backed by `WebGpuTexture` /
///   `SrgbTexture2d`.
/// - `WebGpuTexture` — `crates/frankenterm-gui/src/termwindow/webgpu.rs`;
///   owns `wgpu::Texture`.
/// - `Texture` / `TextureView` — the raw `wgpu::Texture` /
///   `wgpu::TextureView` handles themselves.
pub const FORBIDDEN_LEAVES: &[&str] = &[
    "Sprite",
    "CachedGlyph",
    "Texture2d",
    "WebGpuTexture",
    "Texture",     // wgpu::Texture
    "TextureView", // wgpu::TextureView
];

/// One audit finding: a process-global cache that can transitively pin
/// a GPU resource.
#[derive(Debug, Clone, PartialEq, Eq, Ord, PartialOrd)]
pub struct Finding {
    /// Path relative to the scanned src root.
    pub rel_path: PathBuf,
    /// Source line number (1-based) of the global's declaration.
    pub line: usize,
    /// The global's identifier (e.g. `SHAPED_RUN_INTERNER`).
    pub global: String,
    /// The root type of the global container (e.g. `RefCell<ShapedRunInterner>`).
    pub root_type: String,
    /// The forbidden leaf type reached (e.g. `CachedGlyph`).
    pub forbidden_leaf: String,
    /// The witness path through the type graph, root-first.
    /// e.g. `["RefCell<ShapedRunInterner>", "ShapedRunInterner",
    ///        "InternedShapedRun", "ShapedInfo", "CachedGlyph"]`.
    pub path: Vec<String>,
    pub reason: FindingReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Ord, PartialOrd)]
pub enum FindingReason {
    /// A process-global container's root type transitively reaches a
    /// forbidden GPU-handle leaf.
    GlobalReachesGpuHandle,
}

impl Finding {
    pub fn render(&self) -> String {
        let path = self.path.join(" -> ");
        format!(
            "{}:{}: process-global `{}` : {} can pin a GPU resource — reaches forbidden leaf `{}` ({} (FORBIDDEN))",
            self.rel_path.display(),
            self.line,
            self.global,
            self.root_type,
            self.forbidden_leaf,
            // The path already ends at the leaf; render the full witness.
            path,
        )
    }
}

/// Audit summary returned by [`audit_dir`].
#[derive(Debug, Default, Clone)]
pub struct AuditReport {
    /// Total process-global containers discovered across all dirs.
    pub total_globals: usize,
    /// Globals skipped because they were on the allow-list.
    pub allow_listed_globals: usize,
    /// Globals whose root type does NOT reach a forbidden leaf (clean).
    pub clean_globals: usize,
    /// Distinct type defs ingested into the type graph.
    pub type_graph_nodes: usize,
    pub findings: Vec<Finding>,
}

impl AuditReport {
    pub fn finding_count(&self) -> usize {
        self.findings.len()
    }
}

/// A process-global container declaration discovered in a source file.
#[derive(Debug, Clone)]
struct GlobalSite {
    rel_path: PathBuf,
    /// Forward-slash relative path used for allow-list matching.
    rel_posix: String,
    line: usize,
    ident: String,
    /// The full root type, rendered (e.g. `RefCell<ShapedRunInterner>`).
    root_type_rendered: String,
    /// The set of type idents directly referenced by the root type
    /// (the BFS seed frontier).
    root_type_idents: BTreeSet<String>,
}

/// Walk every scanned dir, build a unified type graph and a global
/// list, then run reachability. `src_roots` may contain several dirs
/// (e.g. the GUI src and the window src); the type graph is built from
/// the union so a global in one dir can reach a leaf defined in another.
pub fn audit_dirs(src_roots: &[PathBuf]) -> Result<AuditReport, std::io::Error> {
    let mut report = AuditReport::default();

    // type_name -> set of referenced type idents (the type graph edges)
    let mut type_graph: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut globals: Vec<GlobalSite> = Vec::new();

    for root in src_roots {
        let mut paths: Vec<PathBuf> = Vec::new();
        for entry in walkdir::WalkDir::new(root) {
            let entry = entry.map_err(std::io::Error::other)?;
            if !entry.file_type().is_file() {
                continue;
            }
            if entry.path().extension().and_then(|s| s.to_str()) != Some("rs") {
                continue;
            }
            paths.push(entry.path().to_path_buf());
        }
        paths.sort();

        for path in &paths {
            let rel = path.strip_prefix(root).unwrap_or(path).to_path_buf();
            ingest_file(path, &rel, &mut type_graph, &mut globals)?;
        }
    }

    report.type_graph_nodes = type_graph.len();
    globals.sort_by(|a, b| {
        (a.rel_posix.clone(), a.line, a.ident.clone()).cmp(&(
            b.rel_posix.clone(),
            b.line,
            b.ident.clone(),
        ))
    });

    for g in &globals {
        report.total_globals += 1;

        if allow_list::ALLOWED_GLOBALS
            .iter()
            .any(|(p, n)| *p == g.rel_posix && *n == g.ident)
        {
            report.allow_listed_globals += 1;
            continue;
        }

        match reaches_forbidden(g, &type_graph) {
            Some((leaf, witness)) => {
                report.findings.push(Finding {
                    rel_path: g.rel_path.clone(),
                    line: g.line,
                    global: g.ident.clone(),
                    root_type: g.root_type_rendered.clone(),
                    forbidden_leaf: leaf,
                    path: witness,
                    reason: FindingReason::GlobalReachesGpuHandle,
                });
            }
            None => {
                report.clean_globals += 1;
            }
        }
    }

    report.findings.sort();
    Ok(report)
}

/// Convenience single-dir wrapper (mirrors `cx_propagation`'s `audit_dir`).
pub fn audit_dir(src_root: &Path) -> Result<AuditReport, std::io::Error> {
    audit_dirs(&[src_root.to_path_buf()])
}

fn ingest_file(
    path: &Path,
    rel: &Path,
    type_graph: &mut BTreeMap<String, BTreeSet<String>>,
    globals: &mut Vec<GlobalSite>,
) -> Result<(), std::io::Error> {
    let src = std::fs::read_to_string(path)?;
    let parsed = match syn::parse_file(&src) {
        Ok(p) => p,
        Err(_) => return Ok(()), // not valid Rust; skip
    };

    let rel_posix = rel.to_string_lossy().replace('\\', "/");
    let mut visitor = FileVisitor {
        rel_path: rel.to_path_buf(),
        rel_posix,
        type_graph,
        globals,
    };
    visitor.visit_file(&parsed);
    Ok(())
}

/// BFS from a global's root-type idents through the type graph. Returns
/// `Some((forbidden_leaf, witness_path))` on the first leaf reached, or
/// `None` if the global is clean. The witness path is root-first and
/// ends at the forbidden leaf.
fn reaches_forbidden(
    g: &GlobalSite,
    type_graph: &BTreeMap<String, BTreeSet<String>>,
) -> Option<(String, Vec<String>)> {
    // Seed: a forbidden leaf may appear DIRECTLY in the root type
    // (e.g. `static X: RefCell<Vec<Sprite>>`), so check the seed idents
    // first. The witness then begins with the rendered root type.
    for ident in &g.root_type_idents {
        if is_forbidden(ident) {
            return Some((ident.clone(), vec![g.root_type_rendered.clone(), ident.clone()]));
        }
    }

    // predecessor map for witness reconstruction
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut pred: BTreeMap<String, String> = BTreeMap::new();
    let mut queue: VecDeque<String> = VecDeque::new();

    for ident in &g.root_type_idents {
        if seen.insert(ident.clone()) {
            queue.push_back(ident.clone());
        }
    }

    while let Some(cur) = queue.pop_front() {
        let Some(neighbors) = type_graph.get(&cur) else {
            continue;
        };
        for next in neighbors {
            if is_forbidden(next) {
                // Reconstruct witness: root_type -> ... -> cur -> next.
                let mut chain = vec![next.clone(), cur.clone()];
                let mut walk = cur.clone();
                while let Some(p) = pred.get(&walk) {
                    chain.push(p.clone());
                    walk = p.clone();
                }
                chain.push(g.root_type_rendered.clone());
                chain.reverse();
                return Some((next.clone(), chain));
            }
            if seen.insert(next.clone()) {
                pred.insert(next.clone(), cur.clone());
                queue.push_back(next.clone());
            }
        }
    }

    None
}

fn is_forbidden(ident: &str) -> bool {
    FORBIDDEN_LEAVES.contains(&ident)
}

struct FileVisitor<'a> {
    rel_path: PathBuf,
    rel_posix: String,
    type_graph: &'a mut BTreeMap<String, BTreeSet<String>>,
    globals: &'a mut Vec<GlobalSite>,
}

impl<'a, 'ast> Visit<'ast> for FileVisitor<'a> {
    fn visit_item_struct(&mut self, node: &'ast ItemStruct) {
        let name = node.ident.to_string();
        let mut idents = BTreeSet::new();
        collect_field_idents(&node.fields, &mut idents);
        self.type_graph.entry(name).or_default().extend(idents);
        syn::visit::visit_item_struct(self, node);
    }

    fn visit_item_enum(&mut self, node: &'ast ItemEnum) {
        let name = node.ident.to_string();
        let mut idents = BTreeSet::new();
        for variant in &node.variants {
            collect_field_idents(&variant.fields, &mut idents);
        }
        self.type_graph.entry(name).or_default().extend(idents);
        syn::visit::visit_item_enum(self, node);
    }

    fn visit_item_type(&mut self, node: &'ast ItemType) {
        // type alias: `type Foo = Bar<Baz>;`
        let name = node.ident.to_string();
        let mut idents = BTreeSet::new();
        collect_type_idents(&node.ty, &mut idents);
        self.type_graph.entry(name).or_default().extend(idents);
        syn::visit::visit_item_type(self, node);
    }

    fn visit_item_static(&mut self, node: &'ast syn::ItemStatic) {
        // top-level `static X: T = ...;`
        let ident = node.ident.to_string();
        let line = node.ident.span().start().line;
        self.record_global(&ident, line, &node.ty);
        syn::visit::visit_item_static(self, node);
    }

    fn visit_item(&mut self, node: &'ast Item) {
        // Skip test-only code — the lint only enforces production code paths.
        // Covers both `#[cfg(test)] mod tests { ... }` and a top-level
        // `#[cfg(test)]`/`#[test]` fn whose body declares a `thread_local!`/
        // `static` (which would otherwise be flagged as a false positive).
        match node {
            Item::Mod(m) if has_cfg_test(&m.attrs) => return,
            Item::Fn(f) if has_cfg_test(&f.attrs) || has_test_attr(&f.attrs) => return,
            _ => {}
        }
        syn::visit::visit_item(self, node);
    }

    fn visit_macro(&mut self, node: &'ast Macro) {
        // thread_local! / lazy_static! expand to statics that the
        // ItemStatic visitor never sees (they live inside the macro
        // token stream). Parse the bodies ourselves.
        let last = node
            .path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_default();
        match last.as_str() {
            "thread_local" => self.parse_thread_local(node),
            "lazy_static" => self.parse_lazy_static(node),
            _ => {}
        }
        syn::visit::visit_macro(self, node);
    }
}

impl<'a> FileVisitor<'a> {
    /// Record a process-global container. For `LazyLock<T>` / `OnceLock<T>`
    /// the *root* is the inner `T` (the cell is just the global wrapper),
    /// but we keep the rendered outer type for the witness header and seed
    /// the BFS with the inner type idents (which include `T`).
    fn record_global(&mut self, ident: &str, line: usize, ty: &Type) {
        let rendered = render_type(ty);
        let mut idents = BTreeSet::new();
        collect_type_idents(ty, &mut idents);
        // `LazyLock`/`OnceLock`/`Lazy`/`OnceCell` are global cells; their
        // inner generic is already captured by collect_type_idents, so the
        // BFS seed naturally includes the payload type. Nothing special to
        // strip — the cell idents themselves simply aren't forbidden and
        // have no outgoing edges in the graph.
        self.globals.push(GlobalSite {
            rel_path: self.rel_path.clone(),
            rel_posix: self.rel_posix.clone(),
            line,
            ident: ident.to_string(),
            root_type_rendered: rendered,
            root_type_idents: idents,
        });
    }

    fn parse_thread_local(&mut self, node: &Macro) {
        // thread_local! body is a sequence of static items:
        //   static X: T = init;
        // Parse the token stream as a block of items.
        for (ident, line, ty) in parse_static_items(&node.tokens) {
            self.record_global(&ident, line, &ty);
        }
    }

    fn parse_lazy_static(&mut self, node: &Macro) {
        // lazy_static! body:
        //   static ref X: T = init;
        for (ident, line, ty) in parse_static_ref_items(&node.tokens) {
            self.record_global(&ident, line, &ty);
        }
    }
}

/// Parse a `thread_local!`-style token stream into `(ident, line, type)`
/// tuples. Each entry is `[#attrs] [pub] static IDENT : TYPE = EXPR ;`.
fn parse_static_items(
    tokens: &proc_macro2::TokenStream,
) -> Vec<(String, usize, Type)> {
    use syn::parse::{Parse, ParseStream};
    use syn::{Token, Visibility};

    struct StaticDecl {
        ident: syn::Ident,
        ty: Type,
    }
    impl Parse for StaticDecl {
        fn parse(input: ParseStream) -> syn::Result<Self> {
            let _attrs = input.call(syn::Attribute::parse_outer)?;
            let _vis: Visibility = input.parse()?;
            input.parse::<Token![static]>()?;
            let ident: syn::Ident = input.parse()?;
            input.parse::<Token![:]>()?;
            let ty: Type = input.parse()?;
            input.parse::<Token![=]>()?;
            // Consume the initializer expression up to the trailing `;`.
            let _init: syn::Expr = input.parse()?;
            input.parse::<Token![;]>()?;
            Ok(StaticDecl { ident, ty })
        }
    }

    struct StaticList(Vec<StaticDecl>);
    impl Parse for StaticList {
        fn parse(input: ParseStream) -> syn::Result<Self> {
            let mut decls = Vec::new();
            while !input.is_empty() {
                decls.push(input.parse()?);
            }
            Ok(StaticList(decls))
        }
    }

    match syn::parse2::<StaticList>(tokens.clone()) {
        Ok(list) => list
            .0
            .into_iter()
            .map(|d| (d.ident.to_string(), d.ident.span().start().line, d.ty))
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Parse a `lazy_static!`-style token stream into `(ident, line, type)`
/// tuples. Each entry is `[#attrs] [pub] static ref IDENT : TYPE = EXPR ;`.
fn parse_static_ref_items(
    tokens: &proc_macro2::TokenStream,
) -> Vec<(String, usize, Type)> {
    use syn::parse::{Parse, ParseStream};
    use syn::{Token, Visibility};

    struct StaticRefDecl {
        ident: syn::Ident,
        ty: Type,
    }
    impl Parse for StaticRefDecl {
        fn parse(input: ParseStream) -> syn::Result<Self> {
            let _attrs = input.call(syn::Attribute::parse_outer)?;
            let _vis: Visibility = input.parse()?;
            input.parse::<Token![static]>()?;
            input.parse::<Token![ref]>()?;
            let ident: syn::Ident = input.parse()?;
            input.parse::<Token![:]>()?;
            let ty: Type = input.parse()?;
            input.parse::<Token![=]>()?;
            let _init: syn::Expr = input.parse()?;
            input.parse::<Token![;]>()?;
            Ok(StaticRefDecl { ident, ty })
        }
    }

    struct StaticRefList(Vec<StaticRefDecl>);
    impl Parse for StaticRefList {
        fn parse(input: ParseStream) -> syn::Result<Self> {
            let mut decls = Vec::new();
            while !input.is_empty() {
                decls.push(input.parse()?);
            }
            Ok(StaticRefList(decls))
        }
    }

    match syn::parse2::<StaticRefList>(tokens.clone()) {
        Ok(list) => list
            .0
            .into_iter()
            .map(|d| (d.ident.to_string(), d.ident.span().start().line, d.ty))
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn collect_field_idents(fields: &Fields, out: &mut BTreeSet<String>) {
    match fields {
        Fields::Named(named) => {
            for f in &named.named {
                collect_type_idents(&f.ty, out);
            }
        }
        Fields::Unnamed(unnamed) => {
            for f in &unnamed.unnamed {
                collect_type_idents(&f.ty, out);
            }
        }
        Fields::Unit => {}
    }
}

/// Collect EVERY path-segment ident appearing anywhere in `ty`,
/// including inside generic arguments, references, boxes, tuples,
/// slices, arrays, and trait-object bounds. This is the edge-extraction
/// that makes `Rc<CachedGlyph>`, `AHashMap<K, Vec<Run>>`,
/// `Option<&Cx>`, `Box<dyn Texture2d>`, etc. all contribute their inner
/// idents as graph edges.
fn collect_type_idents(ty: &Type, out: &mut BTreeSet<String>) {
    match ty {
        Type::Path(p) => collect_path_idents(&p.path, out),
        Type::Reference(r) => collect_type_idents(&r.elem, out),
        Type::Ptr(p) => collect_type_idents(&p.elem, out),
        Type::Slice(s) => collect_type_idents(&s.elem, out),
        Type::Array(a) => collect_type_idents(&a.elem, out),
        Type::Tuple(t) => {
            for elem in &t.elems {
                collect_type_idents(elem, out);
            }
        }
        Type::Group(g) => collect_type_idents(&g.elem, out),
        Type::Paren(p) => collect_type_idents(&p.elem, out),
        Type::TraitObject(to) => {
            // `dyn Texture2d` — the trait name is a leaf ident, and a forbidden
            // leaf can also hide in the trait's generic / associated-type args
            // (`dyn Iterator<Item = Sprite>`), so descend through the whole path.
            for bound in &to.bounds {
                if let syn::TypeParamBound::Trait(tb) = bound {
                    collect_path_idents(&tb.path, out);
                }
            }
        }
        Type::ImplTrait(it) => {
            // `impl Iterator<Item = Sprite>` — identical treatment to `dyn Trait`;
            // the previous code dropped the generic args entirely (false negative).
            for bound in &it.bounds {
                if let syn::TypeParamBound::Trait(tb) = bound {
                    collect_path_idents(&tb.path, out);
                }
            }
        }
        Type::BareFn(bf) => {
            for input in &bf.inputs {
                collect_type_idents(&input.ty, out);
            }
            if let syn::ReturnType::Type(_, rty) = &bf.output {
                collect_type_idents(rty, out);
            }
        }
        _ => {}
    }
}

/// Collect every type-name ident reachable from a `syn::Path`: the segment
/// idents plus everything inside angle-bracketed generic args — both
/// `GenericArgument::Type` and associated-type bindings (`Iterator<Item = T>`) —
/// and parenthesized `Fn(Args) -> Ret` args. Shared by the `Type::Path`,
/// `dyn Trait`, and `impl Trait` arms so a forbidden leaf cannot hide behind any
/// of them (the `dyn`/`impl` arms previously dropped assoc-type / generic args).
fn collect_path_idents(path: &syn::Path, out: &mut BTreeSet<String>) {
    for seg in &path.segments {
        out.insert(seg.ident.to_string());
        match &seg.arguments {
            syn::PathArguments::AngleBracketed(args) => {
                for arg in &args.args {
                    match arg {
                        syn::GenericArgument::Type(inner) => collect_type_idents(inner, out),
                        syn::GenericArgument::AssocType(assoc) => {
                            collect_type_idents(&assoc.ty, out)
                        }
                        _ => {}
                    }
                }
            }
            syn::PathArguments::Parenthesized(p) => {
                for input in &p.inputs {
                    collect_type_idents(input, out);
                }
                if let syn::ReturnType::Type(_, rty) = &p.output {
                    collect_type_idents(rty, out);
                }
            }
            syn::PathArguments::None => {}
        }
    }
}

/// Render a type to a compact string for the finding header / witness.
fn render_type(ty: &Type) -> String {
    use quote::ToTokens;
    let mut s = ty.to_token_stream().to_string();
    // Collapse the whitespace token-stream renders inject around `<`, `,`,
    // `:` so the output reads like source (`RefCell<ShapedRunInterner>`,
    // not `RefCell < ShapedRunInterner >`).
    s = s.replace(" <", "<").replace("< ", "<");
    s = s.replace(" >", ">").replace("> ", ">");
    s = s.replace(" ,", ",");
    s = s.replace(" ::", "::").replace(":: ", "::");
    s
}

/// True only when an item is compiled *exclusively* under `#[cfg(test)]` and so
/// can be skipped. The previous `tokens.to_string().contains("test")` was a
/// correctness bug: it also matched `#[cfg(not(test))]` (production-only code —
/// the exact thing the lint must scan), `#[cfg(any(feature="x", test))]` (built
/// in prod under feature x), and `#[cfg(feature="test-helpers")]`. We parse the
/// predicate and decide test-only precisely.
fn has_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        if !a.path().is_ident("cfg") {
            return false;
        }
        match &a.meta {
            syn::Meta::List(list) => syn::parse2::<syn::Meta>(list.tokens.clone())
                .map(|pred| cfg_is_test_only(&pred))
                .unwrap_or(false),
            _ => false,
        }
    })
}

/// Does this cfg predicate compile the item *only* when `test` is set?
/// - bare `test`                      → yes
/// - `all(..)`                        → yes if ANY conjunct forces test (all() needs every cond)
/// - `any(..)`                        → yes only if EVERY branch forces test (else it builds in prod)
/// - `not(..)`                        → no (e.g. `not(test)` is production code)
/// - `feature = ".."`, `unix`, etc.   → no
fn cfg_is_test_only(meta: &syn::Meta) -> bool {
    match meta {
        syn::Meta::Path(p) => p.is_ident("test"),
        syn::Meta::List(list) => {
            let ident = list
                .path
                .segments
                .last()
                .map(|s| s.ident.to_string())
                .unwrap_or_default();
            let inner: Vec<syn::Meta> = list
                .parse_args_with(
                    syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
                )
                .map(|p| p.into_iter().collect())
                .unwrap_or_default();
            match ident.as_str() {
                "all" => inner.iter().any(cfg_is_test_only),
                "any" => !inner.is_empty() && inner.iter().all(cfg_is_test_only),
                _ => false, // not(..) and any unknown wrapper: not test-only
            }
        }
        syn::Meta::NameValue(_) => false,
    }
}

/// True for `#[test]`, `#[tokio::test]`, `#[async_std::test]`, etc. — used to
/// skip test functions whose bodies declare `thread_local!`/`static` globals
/// (those are not production caches and must not be flagged).
fn has_test_attr(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path()
            .segments
            .last()
            .is_some_and(|s| s.ident == "test")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_graph_and_globals(
        src: &str,
    ) -> (BTreeMap<String, BTreeSet<String>>, Vec<GlobalSite>) {
        let parsed = syn::parse_file(src).unwrap();
        let mut type_graph = BTreeMap::new();
        let mut globals = Vec::new();
        let mut v = FileVisitor {
            rel_path: PathBuf::from("test.rs"),
            rel_posix: "test.rs".to_string(),
            type_graph: &mut type_graph,
            globals: &mut globals,
        };
        v.visit_file(&parsed);
        (type_graph, globals)
    }

    fn first_finding(src: &str) -> Option<Finding> {
        let (graph, globals) = build_graph_and_globals(src);
        for g in &globals {
            if let Some((leaf, witness)) = reaches_forbidden(g, &graph) {
                return Some(Finding {
                    rel_path: g.rel_path.clone(),
                    line: g.line,
                    global: g.ident.clone(),
                    root_type: g.root_type_rendered.clone(),
                    forbidden_leaf: leaf,
                    path: witness,
                    reason: FindingReason::GlobalReachesGpuHandle,
                });
            }
        }
        None
    }

    #[test]
    fn thread_local_reaching_sprite_is_flagged() {
        let src = r#"
            thread_local! {
                static C: RefCell<Interner> = RefCell::new(Interner::default());
            }
            struct Interner { runs: HashMap<u64, Vec<Run>> }
            struct Run { shaped: Vec<Info> }
            struct Info { glyph: Rc<CachedGlyph> }
            struct CachedGlyph { texture: Option<Sprite> }
            struct Sprite { gpu: u32 }
        "#;
        let f = first_finding(src).expect("must flag the global cache");
        assert_eq!(f.global, "C");
        // The first forbidden leaf reached on the BFS is CachedGlyph
        // (it appears before Sprite in the chain).
        assert!(
            f.forbidden_leaf == "CachedGlyph" || f.forbidden_leaf == "Sprite",
            "leaf was {}",
            f.forbidden_leaf
        );
        assert!(f.path.first().unwrap().contains("Interner"));
    }

    #[test]
    fn thread_local_pod_only_is_clean() {
        let src = r#"
            thread_local! {
                static C: RefCell<Interner> = RefCell::new(Interner::default());
            }
            struct Interner { runs: HashMap<u64, Vec<Run>> }
            struct Run { positions: Vec<Pos> }
            struct Pos { x: f32, y: f32, idx: u32 }
        "#;
        assert!(first_finding(src).is_none(), "POD-only global must be clean");
    }

    #[test]
    fn top_level_static_reaching_texture_is_flagged() {
        let src = r#"
            static GLOBAL_CACHE: Holder = Holder;
            struct Holder { t: Box<dyn Texture2d> }
        "#;
        let f = first_finding(src).expect("static reaching Texture2d must flag");
        assert_eq!(f.global, "GLOBAL_CACHE");
        assert_eq!(f.forbidden_leaf, "Texture2d");
    }

    #[test]
    fn lazy_static_reaching_webgpu_texture_is_flagged() {
        let src = r#"
            lazy_static! {
                static ref CACHE: Vec<Entry> = Vec::new();
            }
            struct Entry { tex: WebGpuTexture }
        "#;
        let f = first_finding(src).expect("lazy_static reaching WebGpuTexture must flag");
        assert_eq!(f.global, "CACHE");
        assert_eq!(f.forbidden_leaf, "WebGpuTexture");
    }

    #[test]
    fn lazy_lock_inner_type_is_the_root() {
        let src = r#"
            static CACHE: LazyLock<Holder> = LazyLock::new(Holder::default);
            struct Holder { glyph: Rc<CachedGlyph> }
            struct CachedGlyph { texture: Option<Sprite> }
            struct Sprite { gpu: u32 }
        "#;
        let f = first_finding(src).expect("LazyLock inner must be walked");
        assert_eq!(f.global, "CACHE");
    }

    #[test]
    fn forbidden_leaf_directly_in_root_type_is_flagged() {
        let src = r#"
            thread_local! {
                static C: RefCell<Vec<Sprite>> = RefCell::new(Vec::new());
            }
        "#;
        let f = first_finding(src).expect("direct Sprite in root must flag");
        assert_eq!(f.forbidden_leaf, "Sprite");
    }

    #[test]
    fn local_field_owner_is_not_a_root() {
        // TermWindow owns a GlyphCache that owns CachedGlyph, but TermWindow
        // is NOT a process-global, so there is no finding. This is the
        // legitimate-ownership spare.
        let src = r#"
            struct TermWindow { glyph_cache: GlyphCache }
            struct GlyphCache { glyphs: Vec<Rc<CachedGlyph>> }
            struct CachedGlyph { texture: Option<Sprite> }
            struct Sprite { gpu: u32 }
        "#;
        assert!(
            first_finding(src).is_none(),
            "a non-global field owner must not be flagged"
        );
    }

    #[test]
    fn cfg_test_module_global_is_skipped() {
        let src = r#"
            #[cfg(test)]
            mod tests {
                thread_local! {
                    static C: RefCell<Sprite> = panic!();
                }
            }
        "#;
        let (_g, globals) = build_graph_and_globals(src);
        assert!(globals.is_empty(), "#[cfg(test)] globals must be skipped");
    }

    #[test]
    fn wgpu_raw_texture_is_forbidden() {
        let src = r#"
            static CACHE: Holder = Holder;
            struct Holder { t: wgpu::Texture }
        "#;
        let f = first_finding(src).expect("raw wgpu::Texture must flag");
        assert_eq!(f.forbidden_leaf, "Texture");
    }

    #[test]
    fn enum_variant_reaching_leaf_is_flagged() {
        let src = r#"
            thread_local! {
                static C: RefCell<Node> = panic!();
            }
            enum Node { Empty, Leaf(Sprite), Branch { child: Box<Node> } }
        "#;
        let f = first_finding(src).expect("enum variant payload must be walked");
        assert_eq!(f.forbidden_leaf, "Sprite");
    }

    #[test]
    fn type_alias_edge_is_followed() {
        let src = r#"
            thread_local! {
                static C: RefCell<Alias> = panic!();
            }
            type Alias = Holder;
            struct Holder { t: Sprite }
        "#;
        let f = first_finding(src).expect("type alias must be a graph edge");
        assert_eq!(f.forbidden_leaf, "Sprite");
    }

    #[test]
    fn witness_path_is_root_first_and_ends_at_leaf() {
        let src = r#"
            thread_local! {
                static C: RefCell<Interner> = panic!();
            }
            struct Interner { runs: Vec<Info> }
            struct Info { glyph: Rc<CachedGlyph> }
            struct CachedGlyph { texture: Option<Sprite> }
            struct Sprite { gpu: u32 }
        "#;
        let f = first_finding(src).unwrap();
        assert!(f.path.first().unwrap().contains("Interner"));
        assert_eq!(f.path.last().unwrap(), &f.forbidden_leaf);
        // Path must contain the intermediate node.
        assert!(f.path.iter().any(|n| n == "Info"));
    }

    #[test]
    fn render_type_collapses_whitespace() {
        let ty: Type = syn::parse_str("RefCell<ShapedRunInterner>").unwrap();
        assert_eq!(render_type(&ty), "RefCell<ShapedRunInterner>");
    }

    // --- Regression tests for the fresh-eyes review fixes ---------------------

    #[test]
    fn cfg_not_test_is_production_and_must_be_scanned() {
        // REGRESSION (false negative): `#[cfg(not(test))]` is PRODUCTION code.
        // The old `tokens.contains("test")` skipped it, silently hiding a real
        // GPU-handle-owning global — the worst possible failure for this lint.
        let src = r#"
            #[cfg(not(test))]
            mod prod {
                thread_local! {
                    static C: RefCell<Sprite> = panic!();
                }
            }
        "#;
        let (_g, globals) = build_graph_and_globals(src);
        assert_eq!(globals.len(), 1, "#[cfg(not(test))] is production — must scan");
        let f = first_finding(src).expect("production global reaching Sprite must flag");
        assert_eq!(f.forbidden_leaf, "Sprite");
    }

    #[test]
    fn cfg_any_with_test_and_feature_is_production_under_feature() {
        // `#[cfg(any(test, feature = "x"))]` still compiles in prod under feature
        // x, so it must be scanned (not test-only).
        let src = r#"
            #[cfg(any(test, feature = "debug"))]
            mod maybe_prod {
                thread_local! {
                    static C: RefCell<Sprite> = panic!();
                }
            }
        "#;
        assert!(
            first_finding(src).is_some(),
            "any(test, feature) builds in prod under the feature — must scan"
        );
    }

    #[test]
    fn cfg_all_test_unix_is_test_only_and_skipped() {
        // `#[cfg(all(test, unix))]` only ever compiles under test → skip.
        let src = r#"
            #[cfg(all(test, unix))]
            mod tests {
                thread_local! {
                    static C: RefCell<Sprite> = panic!();
                }
            }
        "#;
        let (_g, globals) = build_graph_and_globals(src);
        assert!(globals.is_empty(), "all(test, ..) is test-only — must skip");
    }

    #[test]
    fn cfg_feature_test_helpers_is_not_the_test_gate() {
        // `#[cfg(feature = "test-helpers")]` is a real production feature whose
        // NAME merely contains "test"; it must be scanned.
        let src = r#"
            #[cfg(feature = "test-helpers")]
            mod helpers {
                thread_local! {
                    static C: RefCell<Sprite> = panic!();
                }
            }
        "#;
        assert!(
            first_finding(src).is_some(),
            "feature = \"test-helpers\" is production — must scan"
        );
    }

    #[test]
    fn forbidden_leaf_in_trait_object_assoc_type_is_flagged() {
        // REGRESSION (false negative): a forbidden leaf hidden in a `dyn Trait`
        // associated-type binding was dropped by the old TraitObject arm.
        let src = r#"
            static CACHE: Holder = Holder;
            struct Holder { it: Box<dyn Iterator<Item = Sprite>> }
        "#;
        let f = first_finding(src).expect("Sprite in dyn assoc-type must flag");
        assert_eq!(f.forbidden_leaf, "Sprite");
    }

    #[test]
    fn forbidden_leaf_via_fn_ptr_return_dyn_assoc_is_flagged() {
        // REGRESSION (false negative): a forbidden leaf reached only through a
        // fn-pointer return type whose `dyn Trait` carries it in an associated
        // type — exercises both the BareFn return descent and the TraitObject
        // assoc-type fix that the old code dropped.
        let src = r#"
            static CACHE: Holder = Holder;
            struct Holder { it: MakeIter }
            type MakeIter = fn() -> Box<dyn Iterator<Item = WebGpuTexture>>;
        "#;
        let f = first_finding(src).expect("WebGpuTexture in iterator item must flag");
        assert_eq!(f.forbidden_leaf, "WebGpuTexture");
    }

    #[test]
    fn cfg_test_fn_local_global_is_not_flagged() {
        // REGRESSION (false positive): a `static`/`thread_local!` inside a
        // `#[cfg(test)]` fn (not a `mod`) was visited and flagged.
        let src = r#"
            #[cfg(test)]
            fn helper() {
                thread_local! {
                    static C: RefCell<Sprite> = panic!();
                }
            }
        "#;
        let (_g, globals) = build_graph_and_globals(src);
        assert!(globals.is_empty(), "#[cfg(test)] fn-local global must be skipped");
    }

    #[test]
    fn test_attr_fn_local_global_is_not_flagged() {
        // REGRESSION (false positive): same, for a `#[test]`/`#[tokio::test]` fn.
        let src = r#"
            #[tokio::test]
            async fn t() {
                thread_local! {
                    static C: RefCell<Sprite> = panic!();
                }
            }
        "#;
        let (_g, globals) = build_graph_and_globals(src);
        assert!(globals.is_empty(), "#[test] fn-local global must be skipped");
    }
}
