//! Smoke tests for `#[lab_runtime_test]`.
//!
//! **Bead:** `br-ft-88mve` (BR-RC-RUNTIME-SEMANTICS.G14.0.cont.macro).
//!
//! These tests exercise the macro itself — they prove that
//! `#[lab_runtime_test]`, `#[lab_runtime_test(seed = N)]`, and
//! `#[lab_runtime_test(config = expr())]` all expand into a
//! valid `#[test] fn` that drives the LabRuntime fixture.
//!
//! The fixture-contract tests in
//! `crates/frankenterm-core/src/test_fixtures/lab_runtime.rs`
//! intentionally call the function form directly — they verify
//! the substrate's semantics (return value, panic on stuck,
//! seed propagation). Migrating those to the macro form would
//! lose access to the returned `LabReport`, so they stay on the
//! function form by design.

use frankenterm_core::test_fixtures::lab_runtime::LabConfig;
use frankenterm_core_test_macros::lab_runtime_test;

#[lab_runtime_test]
async fn macro_default_seed_runs_to_completion() {
    // The body executes inside `lab_runtime_test(|cx| async move
    // { ... })`. The `cx` identifier is bound by the macro and
    // available without any closure declaration.
    cx.checkpoint().expect("Cx should not be cancelled in fresh runtime");
}

#[lab_runtime_test(seed = 1234)]
async fn macro_explicit_seed_runs_to_completion() {
    // With `seed = 1234`, the macro routes through
    // lab_runtime_test_with_seed(1234, ...). The seed is opaque
    // to the test body — we just verify the wiring.
    cx.checkpoint().expect("Cx should not be cancelled");
}

fn smoke_lab_config() -> LabConfig {
    // Constructed from inside this module so the macro's
    // `config = expr()` form has a real expression to wrap.
    LabConfig::new(7) // arbitrary seed
        .with_auto_advance()
        .worker_count(1)
        .max_steps(64)
}

#[lab_runtime_test(config = smoke_lab_config())]
async fn macro_custom_config_runs_to_completion() {
    cx.checkpoint().expect("Cx should not be cancelled");
}

#[lab_runtime_test]
async fn macro_body_can_branch_on_local_state() {
    // Heavier body: prove the macro doesn't constrain the body's
    // shape beyond the `async fn() -> ()` signature. Local
    // variables, branching, and assertions all work as in any
    // other async block.
    let mut counter = 0u32;
    for _ in 0..16 {
        counter += 1;
        cx.checkpoint().expect("Cx should remain cancellation-free");
    }
    assert_eq!(counter, 16);
}
