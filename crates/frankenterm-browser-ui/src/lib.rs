//! Read-only browser fleet viewer for FrankenTerm.
//!
//! A FrankenTUI model of the pane inventory and detection stream, run in the
//! browser through ftui-web's host-driven `StepProgram`. The host page
//! fetches `ft web`'s `/panes` and `/events`, follows `/stream/events`, feeds
//! the bodies to [`FleetRunner`], and paints the flat patches with the
//! FrankenTerm web renderer. The viewer never writes to a pane.
#![forbid(unsafe_code)]

pub mod model;
pub mod runner;
pub mod wire;

#[cfg(target_arch = "wasm32")]
mod wasm;

pub use model::{FleetModel, StreamState, View};
pub use runner::FleetRunner;
#[cfg(target_arch = "wasm32")]
pub use wasm::FleetViewer;
