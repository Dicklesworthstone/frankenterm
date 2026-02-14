// Vendored from WezTerm — suppress cosmetic clippy lints
#![allow(clippy::collapsible_else_if)]
#![allow(clippy::collapsible_if)]
#![allow(clippy::from_over_into)]
#![allow(clippy::io_other_error)]
#![allow(clippy::let_unit_value)]
#![allow(clippy::manual_strip)]
#![allow(clippy::match_like_matches_macro)]
#![allow(clippy::needless_borrow)]
#![allow(clippy::needless_borrows_for_generic_args)]
#![allow(clippy::needless_question_mark)]
#![allow(clippy::needless_return)]
#![allow(clippy::new_without_default)]
#![allow(clippy::single_match)]
#![allow(clippy::type_complexity)]

#[cfg(not(any(feature = "libssh-rs", feature = "ssh2")))]
compile_error!("Either libssh-rs or ssh2 must be enabled!");

mod auth;
mod channelwrap;
mod config;
mod dirwrap;
mod filewrap;
mod host;
mod pty;
mod session;
mod sessioninner;
mod sessionwrap;
mod sftp;
mod sftpwrap;

pub use auth::*;
pub use config::*;
pub use host::*;
pub use pty::*;
pub use session::*;
pub use sftp::error::*;
pub use sftp::types::*;
pub use sftp::*;

// NOTE: Re-exported as is exposed in a public API of this crate
pub use camino::{Utf8Path, Utf8PathBuf};
pub use filedescriptor::FileDescriptor;
pub use portable_pty::{Child, ChildKiller, MasterPty, PtySize};
