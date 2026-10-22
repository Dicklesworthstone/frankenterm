//! The iPhone's client for a FrankenTerm mux server: the same remote mux the
//! Mac attaches to, reached over `ssh host nc -N -U <socket>`. Nothing runs
//! on the remote machine except the mux server already there.
//!
//! The crate does no I/O. The app feeds it the bytes the SSH channel
//! delivers and sends the bytes it returns; in exchange it learns the
//! machine's panes and, for the pane it shows, a stream of ANSI bytes its
//! local terminal renders exactly like a shell's output.

pub mod frame;
pub mod mirror;
pub mod render;
pub mod session;
pub mod wire;

pub use session::{Event, PaneInfo, Session};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("malformed mux frame: {0}")]
    Frame(&'static str),
    #[error("the mux server sent a reply this client cannot read")]
    Schema,
}
