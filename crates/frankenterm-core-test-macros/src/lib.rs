//! Proc-macro layer for `frankenterm_core::test_fixtures::lab_runtime`.
//!
//! **Bead:** `br-ft-88mve` (BR-RC-RUNTIME-SEMANTICS.G14.0.cont.macro).
//! **Substrate:** `crates/frankenterm-core/src/test_fixtures/lab_runtime.rs` —
//! function-style fixture (`lab_runtime_test`,
//! `lab_runtime_test_with_seed`, `lab_runtime_test_with_config`).
//!
//! # The `#[lab_runtime_test]` attribute
//!
//! Wraps an `async fn` test body into a synchronous `#[test]` entry
//! point that drives the body under LabRuntime. Three argument forms:
//!
//! ```ignore
//! use frankenterm_core_test_macros::lab_runtime_test;
//!
//! #[lab_runtime_test]
//! async fn default_seed() {
//!     // Runs under lab_runtime_test (DEFAULT_SEED).
//!     // The `cx` identifier is bound to the runtime's Cx in scope.
//!     let _ = cx.checkpoint();
//! }
//!
//! #[lab_runtime_test(seed = 42)]
//! async fn explicit_seed() {
//!     // Runs under lab_runtime_test_with_seed(42, ...).
//! }
//!
//! #[lab_runtime_test(config = my_config())]
//! async fn custom_config() {
//!     // Runs under lab_runtime_test_with_config(my_config(), ...).
//!     // The `config` expression must evaluate to LabConfig.
//! }
//! ```
//!
//! # Why a macro
//!
//! The function-style fixture requires a `|cx| async move { ... }`
//! closure at every call site. That works but adds two lines of
//! boilerplate to every test. The macro:
//!
//! 1. Lets the test body live at the top of the function (no
//!    closure indentation).
//! 2. Names the `cx` identifier implicitly so the test body can
//!    reference it without a wrapper closure declaration.
//! 3. Selects the right entry point (`lab_runtime_test` /
//!    `_with_seed` / `_with_config`) based on the attribute args.
//!
//! # Migration from the function form
//!
//! The fixture-contract tests in
//! `crates/frankenterm-core/src/test_fixtures/lab_runtime.rs` (the
//! 9 `#[test]` blocks at the bottom of that file) intentionally
//! continue to call the function form directly — they test the
//! substrate's contract, not the macro's wrapping. New tests
//! (and migrations of the function-form tests in unrelated
//! files) should prefer the macro for ergonomics.
//!
//! See `docs/runtime/labruntime-conventions.md` for the migration
//! guidance.

extern crate proc_macro;

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{ItemFn, parse_macro_input};

/// `#[lab_runtime_test]` — wrap an `async fn` test under
/// LabRuntime. See crate docs for the supported argument forms.
#[proc_macro_attribute]
pub fn lab_runtime_test(args: TokenStream, input: TokenStream) -> TokenStream {
    let item_fn = parse_macro_input!(input as ItemFn);

    // ---- Argument parsing -------------------------------------------------
    // The attribute supports at most one of `seed = <expr>` or
    // `config = <expr>`. Both are optional. Anything else is a hard
    // compile-time error.
    let mut seed: Option<syn::Expr> = None;
    let mut config: Option<syn::Expr> = None;
    let arg_parser = syn::meta::parser(|meta| {
        if meta.path.is_ident("seed") {
            if config.is_some() {
                return Err(meta.error("`seed` and `config` are mutually exclusive"));
            }
            seed = Some(meta.value()?.parse::<syn::Expr>()?);
            Ok(())
        } else if meta.path.is_ident("config") {
            if seed.is_some() {
                return Err(meta.error("`seed` and `config` are mutually exclusive"));
            }
            config = Some(meta.value()?.parse::<syn::Expr>()?);
            Ok(())
        } else {
            Err(meta.error("supported keys: `seed = <u64>`, `config = <expr>`"))
        }
    });
    parse_macro_input!(args with arg_parser);

    // ---- Function-shape validation ---------------------------------------
    // The body must be an `async fn` with no parameters and no
    // return type — those constraints come from the wrapped
    // signature `FnOnce(Cx) -> impl Future<Output = ()>`.
    if item_fn.sig.asyncness.is_none() {
        return syn::Error::new_spanned(
            item_fn.sig.fn_token,
            "`#[lab_runtime_test]` requires an `async fn`",
        )
        .to_compile_error()
        .into();
    }
    if !item_fn.sig.inputs.is_empty() {
        return syn::Error::new_spanned(
            &item_fn.sig.inputs,
            "`#[lab_runtime_test]` async fn must take no parameters; \
             use the `cx` identifier bound in scope by the macro",
        )
        .to_compile_error()
        .into();
    }
    if let syn::ReturnType::Type(_, ty) = &item_fn.sig.output {
        // Allow only the unit type explicitly. `-> ()` is fine;
        // anything else (including Result) is not.
        if !matches!(&**ty, syn::Type::Tuple(t) if t.elems.is_empty()) {
            return syn::Error::new_spanned(ty, "`#[lab_runtime_test]` async fn must return `()`")
                .to_compile_error()
                .into();
        }
    }

    // ---- Code generation -------------------------------------------------
    // Re-emit the user's `async fn body` as a sync `#[test] fn`
    // that drives the body under LabRuntime. The user's
    // attributes are preserved, but `async` is stripped from the
    // signature.
    let attrs = &item_fn.attrs;
    let vis = &item_fn.vis;
    let name = &item_fn.sig.ident;
    let block = &item_fn.block;

    // Pick the right entry point + arg list based on parsed args.
    let entrypoint: TokenStream2 = match (&seed, &config) {
        (None, None) => quote! {
            ::frankenterm_core::test_fixtures::lab_runtime::lab_runtime_test(
                |cx| async move {
                    let _ = &cx;
                    #block
                },
            )
        },
        (Some(seed_expr), None) => quote! {
            ::frankenterm_core::test_fixtures::lab_runtime::lab_runtime_test_with_seed(
                #seed_expr,
                |cx| async move {
                    let _ = &cx;
                    #block
                },
            )
        },
        (None, Some(config_expr)) => quote! {
            ::frankenterm_core::test_fixtures::lab_runtime::lab_runtime_test_with_config(
                #config_expr,
                |cx| async move {
                    let _ = &cx;
                    #block
                },
            )
        },
        (Some(_), Some(_)) => unreachable!("mutual exclusion checked at parse time"),
    };

    let expanded = quote! {
        #(#attrs)*
        #[test]
        #vis fn #name() {
            // Discard the LabReport; tests that need to assert on
            // it should call the function form directly. The macro
            // optimizes for the common case where the assertions
            // are inside the async body.
            let _report: ::frankenterm_core::test_fixtures::lab_runtime::LabReport =
                #entrypoint;
        }
    };

    expanded.into()
}
