//! Read-only browser fleet viewer for FrankenTerm.
//!
//! A FrankenTUI model of the pane inventory and detection stream, run in the
//! browser through ftui-web's host-driven StepProgram. The host page fetches
//! existing read endpoints and paints the flat patches with the web renderer.
//! The terminal module separately exposes the shared bounded application codec;
//! it does not open a socket or enable pane control in the fleet viewer.
#![forbid(unsafe_code)]

pub mod model;
pub mod runner;
pub mod terminal;
pub mod wire;

#[cfg(target_arch = "wasm32")]
mod wasm;

pub use model::{FleetModel, StreamState, View};
pub use runner::FleetRunner;
pub use terminal::TerminalClient;
#[cfg(target_arch = "wasm32")]
pub use wasm::{FleetViewer, TerminalWireClient};
