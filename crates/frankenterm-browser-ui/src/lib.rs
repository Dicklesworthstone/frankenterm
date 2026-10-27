//! Browser fleet viewer and transactional terminal model for FrankenTerm.
//!
//! The fleet model renders pane inventory and detections through ftui-web.
//! The terminal client separately validates and atomically applies rich terminal
//! snapshots/deltas, exposing bounded row pages and model-applied ACKs. The host
//! owns networking and painting; these modules never open sockets or send input.
#![forbid(unsafe_code)]

pub mod model;
pub mod runner;
pub mod terminal;
pub mod terminal_model;
pub mod wire;

#[cfg(target_arch = "wasm32")]
mod wasm;

pub use model::{FleetModel, StreamState, View};
pub use runner::FleetRunner;
pub use terminal::TerminalClient;
pub use terminal_model::{ApplyOutcome, TerminalModel, TerminalScreen};
#[cfg(target_arch = "wasm32")]
pub use wasm::{FleetViewer, TerminalWireClient};
