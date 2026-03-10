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

pub mod runtime {
    pub mod channel {
        pub use smol::channel::{bounded, Receiver, RecvError, Sender, TryRecvError};
    }

    pub mod io {
        #[cfg(feature = "async-asupersync")]
        use std::io;

        #[cfg(feature = "async-asupersync")]
        use asupersync::io::AsyncWriteExt as RuntimeAsyncWriteExt;

        // Transitional shim: keep trait usage behind a crate-local path so
        // async runtime migration can switch implementations centrally.
        #[cfg(feature = "async-asupersync")]
        pub use asupersync::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};

        // Transitional shim: keep trait usage behind a crate-local path so
        // async runtime migration can switch implementations centrally.
        #[cfg(not(feature = "async-asupersync"))]
        pub use smol::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

        #[cfg(feature = "async-asupersync")]
        pub trait AsyncWriteExt: AsyncWrite {
            fn write_all<'a>(
                &'a mut self,
                buf: &'a [u8],
            ) -> impl std::future::Future<Output = io::Result<()>> + 'a
            where
                Self: Unpin,
            {
                RuntimeAsyncWriteExt::write_all(self, buf)
            }

            fn flush(&mut self) -> impl std::future::Future<Output = io::Result<()>> + '_
            where
                Self: Unpin,
            {
                RuntimeAsyncWriteExt::flush(self)
            }

            fn close(&mut self) -> impl std::future::Future<Output = io::Result<()>> + '_
            where
                Self: Unpin,
            {
                RuntimeAsyncWriteExt::shutdown(self)
            }
        }

        #[cfg(feature = "async-asupersync")]
        impl<T: AsyncWrite + ?Sized> AsyncWriteExt for T {}
    }

    #[cfg(feature = "async-asupersync")]
    static ASUPERSYNC_RUNTIME: std::sync::LazyLock<asupersync::runtime::Runtime> =
        std::sync::LazyLock::new(|| {
            asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .expect("failed to build frankenterm-ssh asupersync runtime")
        });

    #[cfg(feature = "async-asupersync")]
    pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
        ASUPERSYNC_RUNTIME.block_on(future)
    }

    #[cfg(not(feature = "async-asupersync"))]
    pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
        smol::block_on(future)
    }
}

#[cfg(test)]
mod runtime_migration_guards {
    use std::fs;
    use std::path::{Path, PathBuf};

    fn collect_rust_files(dir: &Path, files: &mut Vec<PathBuf>) {
        let entries = fs::read_dir(dir).expect("read_dir failed");
        for entry in entries {
            let entry = entry.expect("failed to read dir entry");
            let path = entry.path();
            if path.is_dir() {
                collect_rust_files(&path, files);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                files.push(path);
            }
        }
    }

    fn rust_source_files() -> Vec<PathBuf> {
        let mut files = Vec::new();
        let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        collect_rust_files(&src_dir, &mut files);
        files
    }

    fn rust_test_files() -> Vec<PathBuf> {
        let mut files = Vec::new();
        let tests_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
        collect_rust_files(&tests_dir, &mut files);
        files
    }

    fn lib_before_test_module() -> String {
        let lib_rs = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs");
        let source =
            fs::read_to_string(&lib_rs).unwrap_or_else(|_| panic!("failed to read {:?}", lib_rs));
        let marker = "#[cfg(test)]\nmod runtime_migration_guards";
        source
            .find(marker)
            .map_or(source.clone(), |idx| source[..idx].to_owned())
    }

    fn contains_non_comment_token(line: &str, token: &str) -> bool {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            return false;
        }
        line.contains(token)
    }

    #[test]
    fn no_direct_tokio_namespace_in_ssh_sources() {
        for file in rust_source_files() {
            if file.ends_with("src/lib.rs") {
                continue;
            }
            let source =
                fs::read_to_string(&file).unwrap_or_else(|_| panic!("failed to read {:?}", file));
            for (line_no, line) in source.lines().enumerate() {
                assert!(
                    !contains_non_comment_token(line, "tokio::"),
                    "direct tokio usage found at {}:{}",
                    file.display(),
                    line_no + 1
                );
            }
        }
        let lib_prefix = lib_before_test_module();
        for (line_no, line) in lib_prefix.lines().enumerate() {
            assert!(
                !contains_non_comment_token(line, "tokio::"),
                "direct tokio usage found at src/lib.rs:{}",
                line_no + 1
            );
        }

        let cargo_toml = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let manifest =
            fs::read_to_string(&cargo_toml).expect("failed to read frankenterm-ssh Cargo.toml");
        assert!(
            !manifest.lines().any(|line| {
                let trimmed = line.trim_start();
                !trimmed.starts_with('#') && trimmed.starts_with("tokio")
            }),
            "tokio dependency declaration reintroduced in {}",
            cargo_toml.display()
        );
    }

    #[test]
    fn smol_references_are_isolated_to_runtime_shim() {
        for file in rust_source_files() {
            if file.ends_with("src/lib.rs") {
                continue;
            }

            let source =
                fs::read_to_string(&file).unwrap_or_else(|_| panic!("failed to read {:?}", file));
            for (line_no, line) in source.lines().enumerate() {
                assert!(
                    !contains_non_comment_token(line, "smol::"),
                    "direct smol usage must stay in runtime shim ({}:{})",
                    file.display(),
                    line_no + 1
                );
            }
        }
    }

    #[test]
    fn no_direct_smol_namespace_in_ssh_tests() {
        for file in rust_test_files() {
            let source =
                fs::read_to_string(&file).unwrap_or_else(|_| panic!("failed to read {:?}", file));
            for (line_no, line) in source.lines().enumerate() {
                assert!(
                    !contains_non_comment_token(line, "smol::"),
                    "direct smol usage must stay in runtime shim ({}:{})",
                    file.display(),
                    line_no + 1
                );
            }
        }
    }
}
