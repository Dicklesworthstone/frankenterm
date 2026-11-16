//! Dylint `LateLintPass` implementation for the cx-propagation rule.
//!
//! The stable `syn` analyzer in this crate remains the bulk audit and
//! JSON-reporting surface. This module is the compile-time Dylint layer
//! requested by `ft-rca2p`: it walks rustc HIR, reuses the same
//! allow-list, and emits a lint for every explicit-visibility async
//! function that lacks a `Cx` parameter or `RuntimeProof` bound.

#[allow(unused_extern_crates)]
extern crate rustc_driver;
extern crate rustc_errors;
extern crate rustc_hir;
extern crate rustc_lint;
extern crate rustc_session;
extern crate rustc_span;

use crate::allow_list;
use rustc_errors::DiagDecorator;
use rustc_hir as hir;
use rustc_hir::{
    GenericArg, GenericBound, ImplItem, ImplItemKind, Item, ItemKind, QPath, Ty, TyKind,
    WherePredicateKind,
};
use rustc_lint::{LateContext, LateLintPass, LintContext};
use rustc_span::Span;
use std::ffi::CString;

#[unsafe(no_mangle)]
pub extern "C" fn dylint_version() -> *mut std::os::raw::c_char {
    CString::new("0.1.0").unwrap().into_raw()
}

rustc_session::declare_lint! {
    /// ### What it does
    ///
    /// Flags explicitly visible async functions that do not thread a
    /// `Cx` witness or `RuntimeProof` bound through their signature.
    ///
    /// ### Why is this bad?
    ///
    /// FrankenTerm's async runtime model relies on `Cx` propagation for
    /// structured cancellation and runtime-proof sealing. Public async
    /// surfaces without that witness can reintroduce unstructured async
    /// behavior.
    pub CX_PROPAGATION,
    Warn,
    "visible async function missing a Cx parameter or RuntimeProof bound"
}

rustc_session::declare_lint_pass!(LateLintPassImpl => [CX_PROPAGATION]);

#[unsafe(no_mangle)]
pub fn register_lints(_sess: &rustc_session::Session, lint_store: &mut rustc_lint::LintStore) {
    lint_store.register_lints(&[CX_PROPAGATION]);
    lint_store.register_late_pass(|_| Box::new(LateLintPassImpl));
}

impl<'tcx> LateLintPass<'tcx> for LateLintPassImpl {
    fn check_item(&mut self, cx: &LateContext<'tcx>, item: &'tcx Item<'tcx>) {
        let ItemKind::Fn {
            sig,
            ident,
            generics,
            ..
        } = item.kind
        else {
            return;
        };

        check_fn_site(
            cx,
            Some(item.vis_span),
            item.span,
            ident.as_str(),
            &sig,
            generics,
        );
    }

    fn check_impl_item(&mut self, cx: &LateContext<'tcx>, item: &'tcx ImplItem<'tcx>) {
        let ImplItemKind::Fn(sig, _) = item.kind else {
            return;
        };

        check_fn_site(
            cx,
            item.vis_span(),
            item.span,
            item.ident.as_str(),
            &sig,
            item.generics,
        );
    }
}

fn check_fn_site(
    cx: &LateContext<'_>,
    vis_span: Option<Span>,
    item_span: Span,
    fn_name: &str,
    sig: &hir::FnSig<'_>,
    generics: &hir::Generics<'_>,
) {
    let Some(vis_span) = vis_span else {
        return;
    };
    if vis_span.is_dummy() || !sig.header.is_async() {
        return;
    }

    let Some(rel_path) = rel_path_for_span(cx, item_span) else {
        return;
    };
    let file_name = rel_path.rsplit('/').next().unwrap_or(rel_path.as_str());
    if allow_list::EXEMPT_FILES.contains(&file_name) {
        return;
    }
    if allow_list::WRAPPER_EXEMPTIONS
        .iter()
        .any(|(path, name)| *path == rel_path && *name == fn_name)
    {
        return;
    }

    if sig_is_covered(sig, generics) {
        return;
    }

    cx.emit_span_lint(CX_PROPAGATION, item_span, DiagDecorator(|diag| {
        diag.primary_message(format!(
            "pub async fn `{fn_name}` is missing a `Cx` parameter or `RuntimeProof` bound"
        ));
        diag.help(
            "thread `&Cx` through the signature, add a `RuntimeProof` bound, or add a documented wrapper exemption",
        );
    }));
}

fn rel_path_for_span(cx: &LateContext<'_>, span: Span) -> Option<String> {
    let filename = cx
        .sess()
        .source_map()
        .span_to_filename(span)
        .prefer_local_unconditionally()
        .to_string()
        .replace('\\', "/");

    filename
        .split_once("crates/frankenterm-core/src/")
        .map(|(_, rel)| rel.to_owned())
        .or_else(|| {
            filename
                .split_once("tests/fixtures/")
                .map(|(_, rel)| rel.to_owned())
        })
}

fn sig_is_covered(sig: &hir::FnSig<'_>, generics: &hir::Generics<'_>) -> bool {
    sig.decl.inputs.iter().any(type_mentions_cx) || generics_mentions_runtime_proof(generics)
}

fn generics_mentions_runtime_proof(generics: &hir::Generics<'_>) -> bool {
    generics
        .params
        .iter()
        .flat_map(|param| generics.bounds_for_param(param.def_id))
        .any(|pred| bounds_mention_runtime_proof(pred.bounds))
        || generics.predicates.iter().any(|pred| {
            if let WherePredicateKind::BoundPredicate(bound) = pred.kind {
                bounds_mention_runtime_proof(bound.bounds)
            } else {
                false
            }
        })
}

fn bounds_mention_runtime_proof(bounds: hir::GenericBounds<'_>) -> bool {
    bounds.iter().any(|bound| {
        if let GenericBound::Trait(poly_trait_ref) = bound {
            path_last_is(poly_trait_ref.trait_ref.path, "RuntimeProof")
        } else {
            false
        }
    })
}

fn type_mentions_cx(ty: &Ty<'_>) -> bool {
    match ty.kind {
        TyKind::Ref(_, mut_ty) | TyKind::Ptr(mut_ty) => type_mentions_cx(mut_ty.ty),
        TyKind::Path(qpath) => qpath_mentions_cx(qpath),
        TyKind::Tup(types) => types.iter().any(type_mentions_cx),
        TyKind::Slice(inner) | TyKind::Array(inner, _) | TyKind::Pat(inner, _) => {
            type_mentions_cx(inner)
        }
        TyKind::TraitAscription(bounds) => bounds_mention_runtime_proof(bounds),
        TyKind::OpaqueDef(opaque) => bounds_mention_runtime_proof(opaque.bounds),
        _ => false,
    }
}

fn qpath_mentions_cx(qpath: QPath<'_>) -> bool {
    match qpath {
        QPath::Resolved(_, path) => {
            path_last_is(path, "Cx")
                || path.segments.iter().any(|segment| {
                    segment.args.is_some_and(|args| {
                        args.args.iter().any(|arg| {
                            if let GenericArg::Type(inner) = arg {
                                type_mentions_cx((*inner).as_unambig_ty())
                            } else {
                                false
                            }
                        })
                    })
                })
        }
        QPath::TypeRelative(ty, segment) => {
            type_mentions_cx(ty)
                || segment.args.is_some_and(|args| {
                    args.args.iter().any(|arg| {
                        if let GenericArg::Type(inner) = arg {
                            type_mentions_cx((*inner).as_unambig_ty())
                        } else {
                            false
                        }
                    })
                })
        }
    }
}

fn path_last_is(path: &hir::Path<'_>, name: &str) -> bool {
    path.segments
        .last()
        .map(|segment| segment.ident.as_str() == name)
        .unwrap_or(false)
}
