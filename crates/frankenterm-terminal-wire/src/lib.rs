//! Bounded browser terminal application messages shared by native and WASM.
//!
//! This crate handles UTF-8 JSON, not WebSocket framing, authentication, pane
//! authorization, lease arbitration, or terminal effects. Decoding a request
//! does not authorize it. Applied ACKs describe model application, not socket
//! receipt, canvas paint, or execution by a shell. IDs cross JavaScript as
//! strings; native mux PDUs are never exposed by this schema.
#![forbid(unsafe_code)]

mod bounded;
mod codec;
mod protocol;
mod session;
mod snapshot;

pub use bounded::{BoundedVec, Id, Sha256, Text, WireI64, WireU64};
pub use codec::{decode_client, decode_server, encode_client, encode_server};
pub use protocol::*;
pub use session::{BrowserSession, ServerSession};
pub use snapshot::{FrozenSnapshot, MAX_SNAPSHOT_RESOURCES, SnapshotPublication, snapshot_chunk_bytes};

pub const MAX_FRAME_BYTES: usize = 256 * 1024;
pub const MAX_SNAPSHOT_BYTES: u32 = 8 * 1024 * 1024;
pub const MAX_SUBSCRIPTIONS: usize = 64;
pub const MAX_CAPABILITIES: usize = 16;
pub const MAX_COLUMNS: u16 = 512;
pub const MAX_ROWS: u16 = 512;
pub const MAX_SCREEN_CELLS: u32 = 65_536;
pub const MAX_CHUNKS: u16 = 128;
pub const MAX_CHUNK_LINES: usize = 64;
pub const MAX_CHUNK_CELLS: usize = 2_048;
pub const MAX_RESOURCES: usize = 64;
pub const MAX_PASTE_BYTES: usize = 64 * 1024;
pub const PROTOCOL: &str = "ft.terminal.v1";
pub const SUBPROTOCOL: &str = PROTOCOL;

/// Finite content-free failures; never echo a rejected frame or credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireError {
    FrameTooLarge,
    Malformed,
    UnsupportedVersion,
    InvalidIdentifier,
    InvalidNumber,
    InvalidBounds,
    InvalidState,
    UnsupportedCapability,
    NotNegotiated,
    AlreadyNegotiated,
    AssetMismatch,
    IdentityMismatch,
    StaleScope,
    StaleSequence,
    StaleLease,
    LimitExceeded,
    SnapshotIncomplete,
    Denied,
    Indeterminate,
    Unavailable,
}

impl WireError {
    pub const fn code(self) -> &'static str {
        match self {
            Self::FrameTooLarge => "terminal_wire_frame_too_large",
            Self::Malformed => "terminal_wire_malformed",
            Self::UnsupportedVersion => "terminal_wire_unsupported_version",
            Self::InvalidIdentifier => "terminal_wire_invalid_identifier",
            Self::InvalidNumber => "terminal_wire_invalid_number",
            Self::InvalidBounds => "terminal_wire_invalid_bounds",
            Self::InvalidState => "terminal_wire_invalid_state",
            Self::UnsupportedCapability => "terminal_wire_unsupported_capability",
            Self::NotNegotiated => "terminal_wire_not_negotiated",
            Self::AlreadyNegotiated => "terminal_wire_already_negotiated",
            Self::AssetMismatch => "terminal_wire_asset_mismatch",
            Self::IdentityMismatch => "terminal_wire_identity_mismatch",
            Self::StaleScope => "terminal_wire_stale_scope",
            Self::StaleSequence => "terminal_wire_stale_sequence",
            Self::StaleLease => "terminal_wire_stale_lease",
            Self::LimitExceeded => "terminal_wire_limit_exceeded",
            Self::SnapshotIncomplete => "terminal_wire_snapshot_incomplete",
            Self::Denied => "terminal_wire_denied",
            Self::Indeterminate => "terminal_wire_indeterminate",
            Self::Unavailable => "terminal_wire_unavailable",
        }
    }
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for WireError {}
