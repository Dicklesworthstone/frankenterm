//! Shared test infrastructure for frankenterm-core integration tests.
//!
//! Import from integration test files with:
//! ```ignore
//! mod common;
//! use common::lab;
//! use common::fixtures;
//! ```

#[cfg(feature = "asupersync-runtime")]
pub mod lab;

#[cfg(feature = "asupersync-runtime")]
pub mod fixtures;
