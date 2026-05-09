//! Demonstration tests for the `asupersync_test!` macro
//! (ft-i2eni.2 / BR-RC-DOCTRINE.G1.2).
//!
//! Three integration tests proving the macro:
//!   1. expands to a `#[test]` fn that runs under
//!      `RuntimeFixture::current_thread().block_on(...)` (so async
//!      bodies work);
//!   2. forwards attributes (e.g. `#[ignore]`) onto the generated
//!      `#[test]` fn;
//!   3. tolerates async ops + assertions in the body identically to
//!      the legacy `RuntimeFixture::current_thread().block_on(async {…})`
//!      form the existing 60+ `*_labruntime.rs` tests use.
//!
//! These tests are intentionally trivial (no DB, no network, no
//! storage) — the goal is to pin the macro's compile + runtime
//! contract so subsequent labruntime ports can adopt the macro
//! without re-validating it.

#![cfg(feature = "asupersync-runtime")]

mod common;

#[allow(unused_imports)]
use common::asupersync_test;
use common::fixtures::RuntimeFixture;

asupersync_test! {
    fn macro_runs_async_body_to_completion() {
        let value = async { 42 }.await;
        assert_eq!(value, 42, "async body must execute under RuntimeFixture");
    }
}

asupersync_test! {
    fn macro_threads_async_awaits_in_sequence() {
        let a = async { 1 }.await;
        let b = async { 2 }.await;
        let c = async { 3 }.await;
        assert_eq!(a + b + c, 6);
    }
}

asupersync_test! {
    #[ignore]
    fn macro_forwards_attributes_to_generated_test_fn() {
        // This body intentionally never runs (#[ignore]); the
        // assertion exists to confirm the macro does NOT silently
        // drop the body. If the attribute were lost, the test
        // would run by default and fail.
        panic!("ignored test body must not run");
    }
}
