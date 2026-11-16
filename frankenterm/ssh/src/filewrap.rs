use crate::sftp::types::Metadata;
use crate::sftp::{SftpChannelError, SftpChannelResult};
use camino::{Utf8Path, Utf8PathBuf};

pub(crate) enum FileWrap {
    #[cfg(feature = "ssh2")]
    Ssh2(ssh2::File),

    #[cfg(feature = "libssh-rs")]
    // libssh-rs exposes path-based metadata mutation/stat, so libssh-backed
    // file handles retain their opened path for follow-up metadata operations.
    LibSsh {
        file: libssh_rs::SftpFile,
        path: Utf8PathBuf,
    },
}

impl FileWrap {
    pub fn reader(&mut self) -> Box<dyn std::io::Read + '_> {
        match self {
            #[cfg(feature = "ssh2")]
            Self::Ssh2(file) => Box::new(file),

            #[cfg(feature = "libssh-rs")]
            Self::LibSsh { file, .. } => Box::new(file),
        }
    }

    pub fn writer(&mut self) -> Box<dyn std::io::Write + '_> {
        match self {
            #[cfg(feature = "ssh2")]
            Self::Ssh2(file) => Box::new(file),

            #[cfg(feature = "libssh-rs")]
            Self::LibSsh { file, .. } => Box::new(file),
        }
    }

    #[cfg(feature = "libssh-rs")]
    pub fn libssh_path(&self) -> Option<&Utf8Path> {
        match self {
            Self::LibSsh { path, .. } => Some(path.as_path()),

            #[cfg(feature = "ssh2")]
            Self::Ssh2(_) => None,
        }
    }

    pub fn set_metadata(
        &mut self,
        #[cfg_attr(not(feature = "ssh2"), allow(unused_variables))] metadata: Metadata,
    ) -> SftpChannelResult<()> {
        match self {
            #[cfg(feature = "ssh2")]
            Self::Ssh2(file) => Ok(file.setstat(metadata.into())?),

            #[cfg(feature = "libssh-rs")]
            Self::LibSsh { .. } => Err(libssh_rs::Error::fatal(
                "libssh-backed file metadata mutation must be routed via path-based SFTP set_metadata",
            )
            .into()),
        }
    }

    pub fn metadata(&mut self) -> SftpChannelResult<Metadata> {
        match self {
            #[cfg(feature = "ssh2")]
            Self::Ssh2(file) => Ok(file.stat().map(Metadata::from)?),

            #[cfg(feature = "libssh-rs")]
            Self::LibSsh { file, .. } => file
                .metadata()
                .map(Metadata::from)
                .map_err(SftpChannelError::from),
        }
    }

    pub fn fsync(&mut self) -> SftpChannelResult<()> {
        match self {
            #[cfg(feature = "ssh2")]
            Self::Ssh2(file) => file.fsync().map_err(SftpChannelError::from),

            #[cfg(feature = "libssh-rs")]
            Self::LibSsh { file, .. } => {
                use std::io::Write;
                Ok(file.flush()?)
            }
        }
    }
}
