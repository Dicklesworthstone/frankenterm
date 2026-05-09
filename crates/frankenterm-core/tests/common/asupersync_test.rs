//! `asupersync_test!` — declarative macro for writing async tests
//! without per-test `RuntimeFixture::current_thread().block_on(async {…})`
//! boilerplate.
//!
//! **Bead:** ft-i2eni.2 (BR-RC-DOCTRINE.G1.2). Ships the macro that
//! makes the 60-site Tokio-test-attribute → LabRuntime port (already
//! completed under wa-22x4r) ergonomic going forward. Each existing
//! `*_labruntime.rs` test still wraps its body in
//! `RuntimeFixture::current_thread().block_on(async { … })`; this
//! macro is a drop-in replacement that hides the wrapper while
//! preserving the asupersync runtime + Cx propagation guarantees.
//!
//! # Why a `macro_rules!` macro and not a proc-macro
//!
//! A bang-macro (`asupersync_test!`) is enough for the call-site
//! ergonomics the bead asks for, doesn't require a new workspace
//! crate (proc-macros must live in a `proc_macro = true` crate),
//! and doesn't add `syn` / `quote` to the build graph. The bead's
//! title spells the macro with a `!` (`asupersync_test!`), matching
//! the macro_rules! call form.
//!
//! # Usage
//!
//! ```ignore
//! mod common;
//! use common::asupersync_test;
//!
//! asupersync_test! {
//!     fn my_async_test() {
//!         let storage = setup_storage("my_test").await;
//!         // … assertions on async ops …
//!         teardown(storage).await;
//!     }
//! }
//! ```
//!
//! Expands to:
//!
//! ```ignore
//! #[test]
//! fn my_async_test() {
//!     common::fixtures::RuntimeFixture::current_thread().block_on(async move {
//!         let storage = setup_storage("my_test").await;
//!         // …
//!         teardown(storage).await;
//!     });
//! }
//! ```
//!
//! # Variants
//!
//! - `asupersync_test!{ fn name() { … } }` — runs under the default
//!   `RuntimeFixture::current_thread()`.
//! - `asupersync_test!{ #[ignore] fn name() { … } }` — `#[ignore]`
//!   (or any other attribute) on the inner fn is forwarded to the
//!   generated `#[test]` fn.
//!
//! # Design notes
//!
//! - The macro is intentionally simple: no support for parametrised
//!   arguments, no `start_paused` flag, no return-type coercion.
//!   The 60 ports under `tests/*_labruntime.rs` already use the
//!   `block_on(async { … })` form for free-form bodies; this macro
//!   matches that surface exactly so the conversion is a textual
//!   substitution.
//! - Compatible with the asupersync-runtime feature gate. Callers
//!   that opt out of `asupersync-runtime` simply don't pull this
//!   module in; the macro itself doesn't reference any feature-gated
//!   types directly — the `RuntimeFixture` path is resolved at the
//!   call site, so a `--no-default-features` build stays clean.
//!
//! # Cross-references
//!
//! - `common::fixtures::RuntimeFixture` — the underlying async
//!   fixture the macro delegates to.
//! - `tests/wa_22x4r_no_tokio_test_in_supported_paths.rs` — the
//!   regression guard that pins the "no Tokio test attribute in
//!   supported paths" invariant.

/// Declarative macro that wraps an async-bodied test in the
/// `RuntimeFixture::current_thread().block_on(...)` runner.
///
/// The first form accepts a plain `fn name() { … }`; the second
/// accepts a leading attribute list (e.g. `#[ignore]`) that is
/// forwarded onto the generated `#[test]` fn.
///
/// # Required imports
///
/// The macro names `RuntimeFixture` directly so the caller must have
/// brought it into scope:
///
/// ```ignore
/// mod common;
/// use common::fixtures::RuntimeFixture;
/// use common::asupersync_test;
/// ```
///
/// This mirrors the pattern every existing `*_labruntime.rs` test
/// already uses, so the macro is a drop-in replacement.
#[macro_export]
macro_rules! asupersync_test {
    // Form: asupersync_test!{ fn name() { … } }
    (fn $name:ident () $body:block) => {
        #[test]
        fn $name() {
            RuntimeFixture::current_thread().block_on(async move { $body });
        }
    };
    // Form: asupersync_test!{ #[attr]+ fn name() { … } }
    ($(#[$attr:meta])+ fn $name:ident () $body:block) => {
        #[test]
        $(#[$attr])+
        fn $name() {
            RuntimeFixture::current_thread().block_on(async move { $body });
        }
    };
}

// Re-export so a `use common::asupersync_test;` brings the bang-macro
// into scope at the call site.
#[allow(unused_imports)]
pub use asupersync_test;
