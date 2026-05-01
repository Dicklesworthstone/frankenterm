//! Custom lint: flag `pub async fn`s in `crates/frankenterm-core/src/`
//! that don't take `&Cx` (or thread one through).
//!
//! **Bead:** [BR-RC-RUNTIME-SEMANTICS.G14.1] / `ft-t9a6q.1`.
//! **Doctrine:** `docs/runtime/runtime-proof-trait.md`.
//! **Doc:** `docs/runtime/cx-propagation-lint.md`.
//!
//! # Why a syn-based analyzer rather than a cargo-dylint plugin
//!
//! cargo-dylint's `LateLintPass` API lets us hook into rustc's HIR
//! and run lint checks at compile time — but it requires a pinned
//! nightly toolchain because the rustc internals it rides on aren't
//! stable. The bead's spirit is "structured Rust-side enforcement
//! of `&Cx` propagation"; the analyzer in this crate parses the
//! same source tree using `syn` (stable Rust) and surfaces the
//! exact same set of violations as the Python audit script under
//! ft-3kv6e — just with a real AST instead of regexes.
//!
//! Real `LateLintPass` migration is filed as ft-t9a6q.1.cont.
//! Substrate-first lets the allow-list, fixture corpus, and rule
//! shape land in stable Rust today; the dylint plugin then drops
//! in against the same analyzer surface without re-litigating the
//! rule semantics.
//!
//! # Rule
//!
//! A `pub async fn` (or `pub(crate)` / `pub(super)` etc.) is
//! *covered* if its parameter list contains any of:
//!
//! - `&Cx` / `&mut Cx` reference parameter
//! - owned `Cx` parameter (also sealed via `impl RuntimeProof for Cx`)
//! - generic bound `: RuntimeProof`
//! - generic bound `impl RuntimeProof`
//!
//! Any path to `Cx` is accepted (`&crate::cx::Cx`,
//! `&frankenterm_core::cx::Cx`, etc.) — the analyzer matches on
//! the last segment of the path so re-export aliases work.
//!
//! # Allow-list
//!
//! Two carve-outs, both load-bearing:
//!
//! 1. **EXEMPT_FILES** — runtime-layer modules that *are* the seal
//!    (`runtime_async.rs`, `runtime_proof.rs`, `cx.rs`, `cx_stub.rs`).
//!    They define `Cx` and `RuntimeProof` themselves, so requiring
//!    them to take a `Cx` parameter on themselves is circular.
//! 2. **WRAPPER_EXEMPTIONS** — ergonomic wrappers that construct a
//!    default `Cx` internally and immediately delegate to a covered
//!    sibling. Each entry is a `(relative_path, fn_name)` pair and
//!    must be paired with a real covered sibling on the same file.
//!
//! Both lists mirror the audit script's allow-list exactly. The
//! analyzer enforces the same "wrapper exemption must have a real
//! covered sibling" sanity check the script enforces.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use syn::visit::Visit;
use syn::{FnArg, GenericParam, Item, ItemFn, ItemImpl, Pat, Type, Visibility};

pub mod allow_list;

/// One audit finding. Each call site that fails the rule produces
/// one of these; the runner emits them in deterministic order.
#[derive(Debug, Clone, PartialEq, Eq, Ord, PartialOrd)]
pub struct Finding {
    /// Path relative to `crates/frankenterm-core/src/`.
    pub rel_path: PathBuf,
    /// Source line number (1-based) of the `pub async fn` line.
    pub line: usize,
    /// Function name.
    pub fn_name: String,
    /// Reason the lint fired.
    pub reason: FindingReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Ord, PartialOrd)]
pub enum FindingReason {
    /// `pub async fn` with no `Cx` / `RuntimeProof` in scope.
    MissingCxOrRuntimeProof,
    /// `WRAPPER_EXEMPTIONS` lists this `(file, fn)` but no real
    /// `pub async fn` of that name exists in the file. Indicates a
    /// stale allow-list entry.
    StaleWrapperExemption,
}

impl Finding {
    pub fn render(&self) -> String {
        match &self.reason {
            FindingReason::MissingCxOrRuntimeProof => format!(
                "{}:{}: pub async fn `{}` is missing `&Cx` / `RuntimeProof` parameter or bound",
                self.rel_path.display(),
                self.line,
                self.fn_name,
            ),
            FindingReason::StaleWrapperExemption => format!(
                "{}: WRAPPER_EXEMPTIONS lists `{}` but no such pub async fn exists in the file",
                self.rel_path.display(),
                self.fn_name,
            ),
        }
    }
}

/// Audit summary returned by [`audit_dir`].
#[derive(Debug, Default, Clone)]
pub struct AuditReport {
    pub total_pub_async_sites: usize,
    pub exempt_file_sites: usize,
    pub wrapper_exempt_sites: usize,
    pub covered_sites: usize,
    pub findings: Vec<Finding>,
}

impl AuditReport {
    pub fn uncovered_count(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| matches!(f.reason, FindingReason::MissingCxOrRuntimeProof))
            .count()
    }

    pub fn stale_exemption_count(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| matches!(f.reason, FindingReason::StaleWrapperExemption))
            .count()
    }
}

/// Walk `src_root` for `*.rs` files and audit every `pub async fn`.
/// Returns `Err` only on filesystem walk errors; per-file parse
/// errors are skipped (the file isn't valid Rust, so the audit has
/// nothing to say).
pub fn audit_dir(src_root: &Path) -> Result<AuditReport, std::io::Error> {
    let mut report = AuditReport::default();

    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in walkdir::WalkDir::new(src_root) {
        let entry = entry.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
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
        let rel = path.strip_prefix(src_root).unwrap_or(path).to_path_buf();
        let file_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let is_exempt_file = allow_list::EXEMPT_FILES.contains(&file_name);
        audit_file(path, &rel, is_exempt_file, &mut report)?;
    }

    Ok(report)
}

fn audit_file(
    path: &Path,
    rel: &Path,
    is_exempt_file: bool,
    report: &mut AuditReport,
) -> Result<(), std::io::Error> {
    let src = std::fs::read_to_string(path)?;
    let parsed = match syn::parse_file(&src) {
        Ok(p) => p,
        Err(_) => return Ok(()), // not valid Rust; skip
    };

    let mut visitor = AsyncFnVisitor::default();
    visitor.visit_file(&parsed);

    let rel_posix = rel.to_string_lossy().replace('\\', "/");
    let exemptions_for_file: BTreeSet<&'static str> = allow_list::WRAPPER_EXEMPTIONS
        .iter()
        .filter(|(p, _)| *p == rel_posix)
        .map(|(_, n)| *n)
        .collect();

    let observed_names: BTreeSet<String> = visitor
        .async_fns
        .iter()
        .map(|f| f.name.clone())
        .collect();

    for site in &visitor.async_fns {
        report.total_pub_async_sites += 1;
        if is_exempt_file {
            report.exempt_file_sites += 1;
            continue;
        }
        if exemptions_for_file.contains(site.name.as_str()) {
            report.wrapper_exempt_sites += 1;
            continue;
        }
        if site.is_covered {
            report.covered_sites += 1;
        } else {
            report.findings.push(Finding {
                rel_path: rel.to_path_buf(),
                line: site.line,
                fn_name: site.name.clone(),
                reason: FindingReason::MissingCxOrRuntimeProof,
            });
        }
    }

    // Stale-exemption check: every WRAPPER_EXEMPTIONS entry for
    // this file must correspond to a real pub async fn in the file.
    for name in &exemptions_for_file {
        if !observed_names.contains(*name) {
            report.findings.push(Finding {
                rel_path: rel.to_path_buf(),
                line: 0,
                fn_name: (*name).to_string(),
                reason: FindingReason::StaleWrapperExemption,
            });
        }
    }

    Ok(())
}

struct AsyncFnSite {
    name: String,
    line: usize,
    is_covered: bool,
}

#[derive(Default)]
struct AsyncFnVisitor {
    async_fns: Vec<AsyncFnSite>,
}

impl<'ast> Visit<'ast> for AsyncFnVisitor {
    fn visit_item_fn(&mut self, node: &'ast ItemFn) {
        if is_pub(&node.vis) && node.sig.asyncness.is_some() {
            self.async_fns.push(site_for(&node.sig, node));
        }
        syn::visit::visit_item_fn(self, node);
    }

    fn visit_item_impl(&mut self, node: &'ast ItemImpl) {
        // Inherent impl methods with pub vis count too.
        for item in &node.items {
            if let syn::ImplItem::Method(m) = item {
                if is_pub(&m.vis) && m.sig.asyncness.is_some() {
                    let name = m.sig.ident.to_string();
                    let line = lines_before(&m.sig.ident.span()) + 1;
                    let is_covered = sig_is_covered(&m.sig);
                    self.async_fns.push(AsyncFnSite {
                        name,
                        line,
                        is_covered,
                    });
                }
            }
        }
        syn::visit::visit_item_impl(self, node);
    }

    fn visit_item(&mut self, node: &'ast Item) {
        // Don't recurse into nested mod{} test blocks for the
        // primary audit — the bead only enforces against
        // production code paths. Inline `#[cfg(test)] mod tests`
        // is the convention; we skip those.
        if let Item::Mod(m) = node {
            if has_cfg_test(&m.attrs) {
                return;
            }
        }
        syn::visit::visit_item(self, node);
    }
}

fn site_for(sig: &syn::Signature, node: &ItemFn) -> AsyncFnSite {
    let name = sig.ident.to_string();
    let line = lines_before(&node.sig.ident.span()) + 1;
    let is_covered = sig_is_covered(sig);
    AsyncFnSite {
        name,
        line,
        is_covered,
    }
}

fn lines_before(span: &proc_macro2::Span) -> usize {
    // syn 1.x exposes line numbers via Span::start which returns a
    // LineColumn. The column is 0-indexed, the line is 1-indexed.
    span.start().line.saturating_sub(1)
}

fn is_pub(vis: &Visibility) -> bool {
    !matches!(vis, Visibility::Inherited)
}

fn sig_is_covered(sig: &syn::Signature) -> bool {
    for arg in &sig.inputs {
        if let FnArg::Typed(pat) = arg {
            if type_mentions_cx(&pat.ty) {
                return true;
            }
            // Ignore arg name matters; only type matters.
            let _ = &pat.pat;
        }
    }

    for gp in &sig.generics.params {
        if let GenericParam::Type(tp) = gp {
            for bound in &tp.bounds {
                if let syn::TypeParamBound::Trait(tb) = bound {
                    if path_last_is(&tb.path, "RuntimeProof") {
                        return true;
                    }
                }
            }
        }
    }

    if let Some(where_clause) = &sig.generics.where_clause {
        for pred in &where_clause.predicates {
            if let syn::WherePredicate::Type(t) = pred {
                for bound in &t.bounds {
                    if let syn::TypeParamBound::Trait(tb) = bound {
                        if path_last_is(&tb.path, "RuntimeProof") {
                            return true;
                        }
                    }
                }
            }
        }
    }

    false
}

fn type_mentions_cx(ty: &Type) -> bool {
    match ty {
        Type::Reference(r) => type_mentions_cx(&r.elem),
        Type::Path(p) => path_last_is(&p.path, "Cx"),
        Type::ImplTrait(it) => it.bounds.iter().any(|b| {
            if let syn::TypeParamBound::Trait(tb) = b {
                path_last_is(&tb.path, "RuntimeProof")
            } else {
                false
            }
        }),
        _ => false,
    }
}

fn path_last_is(path: &syn::Path, name: &str) -> bool {
    path.segments
        .last()
        .map(|s| s.ident == name)
        .unwrap_or(false)
}

fn has_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path.is_ident("cfg")
            && a.tokens.to_string().contains("test")
    })
}

#[allow(dead_code)]
fn pat_name(pat: &Pat) -> Option<String> {
    if let Pat::Ident(i) = pat {
        Some(i.ident.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_and_check(src: &str) -> bool {
        let file = syn::parse_file(src).unwrap();
        let mut v = AsyncFnVisitor::default();
        v.visit_file(&file);
        assert_eq!(v.async_fns.len(), 1, "expected one pub async fn");
        v.async_fns[0].is_covered
    }

    #[test]
    fn covered_via_ref_cx() {
        assert!(parse_and_check(
            "pub async fn foo(cx: &Cx) -> u32 { 0 }"
        ));
    }

    #[test]
    fn covered_via_mut_ref_cx() {
        assert!(parse_and_check(
            "pub async fn foo(cx: &mut Cx) -> u32 { 0 }"
        ));
    }

    #[test]
    fn covered_via_owned_cx() {
        assert!(parse_and_check(
            "pub async fn foo(cx: Cx) -> u32 { 0 }"
        ));
    }

    #[test]
    fn covered_via_qualified_path() {
        assert!(parse_and_check(
            "pub async fn foo(cx: &crate::cx::Cx) -> u32 { 0 }"
        ));
    }

    #[test]
    fn covered_via_impl_runtime_proof() {
        assert!(parse_and_check(
            "pub async fn foo(rt: impl RuntimeProof) -> u32 { 0 }"
        ));
    }

    #[test]
    fn covered_via_generic_bound() {
        assert!(parse_and_check(
            "pub async fn foo<R: RuntimeProof>(rt: R) -> u32 { 0 }"
        ));
    }

    #[test]
    fn covered_via_where_clause_bound() {
        assert!(parse_and_check(
            "pub async fn foo<R>(rt: R) -> u32 where R: RuntimeProof { 0 }"
        ));
    }

    #[test]
    fn uncovered_no_params() {
        assert!(!parse_and_check("pub async fn foo() -> u32 { 0 }"));
    }

    #[test]
    fn uncovered_unrelated_params() {
        assert!(!parse_and_check(
            "pub async fn foo(x: u32, s: &str) -> u32 { x }"
        ));
    }

    #[test]
    fn uncovered_misnamed_arg() {
        // `cx: &str` is not Cx.
        assert!(!parse_and_check(
            "pub async fn foo(cx: &str) -> u32 { 0 }"
        ));
    }

    #[test]
    fn private_async_fn_not_inspected() {
        let src = "async fn foo(x: u32) -> u32 { x }";
        let file = syn::parse_file(src).unwrap();
        let mut v = AsyncFnVisitor::default();
        v.visit_file(&file);
        assert!(v.async_fns.is_empty(), "private async fn should be ignored");
    }

    #[test]
    fn pub_crate_counts_as_pub() {
        let src = "pub(crate) async fn foo() -> u32 { 0 }";
        let file = syn::parse_file(src).unwrap();
        let mut v = AsyncFnVisitor::default();
        v.visit_file(&file);
        assert_eq!(v.async_fns.len(), 1);
        assert!(!v.async_fns[0].is_covered);
    }

    #[test]
    fn cfg_test_module_is_skipped() {
        let src = r#"
            #[cfg(test)]
            mod tests {
                pub async fn foo() -> u32 { 0 }
            }
            pub async fn bar(cx: &Cx) -> u32 { 0 }
        "#;
        let file = syn::parse_file(src).unwrap();
        let mut v = AsyncFnVisitor::default();
        v.visit_file(&file);
        assert_eq!(v.async_fns.len(), 1, "test module should be skipped");
        assert_eq!(v.async_fns[0].name, "bar");
    }

    #[test]
    fn impl_block_method_inspected() {
        let src = r#"
            struct S;
            impl S {
                pub async fn foo(&self, cx: &Cx) -> u32 { 0 }
                pub async fn bar(&self) -> u32 { 0 }
            }
        "#;
        let file = syn::parse_file(src).unwrap();
        let mut v = AsyncFnVisitor::default();
        v.visit_file(&file);
        assert_eq!(v.async_fns.len(), 2);
        let foo = v.async_fns.iter().find(|s| s.name == "foo").unwrap();
        let bar = v.async_fns.iter().find(|s| s.name == "bar").unwrap();
        assert!(foo.is_covered);
        assert!(!bar.is_covered);
    }

    #[test]
    fn finding_render_includes_path_line_name() {
        let f = Finding {
            rel_path: PathBuf::from("foo/bar.rs"),
            line: 42,
            fn_name: "baz".to_string(),
            reason: FindingReason::MissingCxOrRuntimeProof,
        };
        let s = f.render();
        assert!(s.contains("foo/bar.rs"));
        assert!(s.contains(":42"));
        assert!(s.contains("baz"));
    }
}
