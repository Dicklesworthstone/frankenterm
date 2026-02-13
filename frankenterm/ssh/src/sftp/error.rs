use std::convert::TryFrom;
use thiserror::Error;

/// Represents a result whose error is [`SftpError`]
pub type SftpResult<T> = Result<T, SftpError>;

/// Represents errors associated with sftp operations
#[derive(Copy, Clone, Debug, Error, Hash, PartialEq, Eq)]
pub enum SftpError {
    // Following are available on libssh and libssh2
    #[error("End-of-file encountered")]
    Eof = 1,
    #[error("File doesn't exist")]
    NoSuchFile = 2,
    #[error("Permission denied")]
    PermissionDenied = 3,
    #[error("Generic failure")]
    Failure = 4,
    #[error("Garbage received from server")]
    BadMessage = 5,
    #[error("No connection has been set up")]
    NoConnection = 6,
    #[error("There was a connection, but we lost it")]
    ConnectionLost = 7,
    #[error("Operation not supported by the server")]
    OpUnsupported = 8,
    #[error("Invalid file handle")]
    InvalidHandle = 9,
    #[error("No such file or directory path exists")]
    NoSuchPath = 10,
    #[error("An attempt to create an already existing file or directory has been made")]
    FileAlreadyExists = 11,
    #[error("We are trying to write on a write-protected filesystem")]
    WriteProtect = 12,
    #[error("No media in remote drive")]
    NoMedia = 13,

    // Below are libssh2-specific errors
    #[cfg(feature = "ssh2")]
    #[error("No space available on filesystem")]
    NoSpaceOnFilesystem = 14,
    #[cfg(feature = "ssh2")]
    #[error("Quota exceeded")]
    QuotaExceeded = 15,
    #[cfg(feature = "ssh2")]
    #[error("Unknown principal")]
    UnknownPrincipal = 16,
    #[cfg(feature = "ssh2")]
    #[error("Filesystem lock conflict")]
    LockConflict = 17,
    #[cfg(feature = "ssh2")]
    #[error("Directory is not empty")]
    DirNotEmpty = 18,
    #[cfg(feature = "ssh2")]
    #[error("Operation attempted against a path that is not a directory")]
    NotADirectory = 19,
    #[cfg(feature = "ssh2")]
    #[error("Filename invalid")]
    InvalidFilename = 20,
    #[cfg(feature = "ssh2")]
    #[error("Symlink loop encountered")]
    LinkLoop = 21,
}

impl SftpError {
    /// Produces an SFTP error from the given code if it matches a known error type
    pub fn from_error_code(code: i32) -> Option<SftpError> {
        Self::try_from(code).ok()
    }

    /// Converts into an error code
    pub fn to_error_code(self) -> i32 {
        self as i32
    }
}

impl TryFrom<i32> for SftpError {
    type Error = Result<(), i32>;

    /// Attempt to convert an arbitrary code to an sftp error, returning
    /// `Ok` if matching an sftp error or `Err` if the code represented a
    /// success or was unknown
    fn try_from(code: i32) -> Result<Self, Self::Error> {
        match code {
            // 0 means okay in libssh and libssh2, which isn't an error
            0 => Err(Ok(())),

            1 => Ok(Self::Eof),
            2 => Ok(Self::NoSuchFile),
            3 => Ok(Self::PermissionDenied),
            4 => Ok(Self::Failure),
            5 => Ok(Self::BadMessage),
            6 => Ok(Self::NoConnection),
            7 => Ok(Self::ConnectionLost),
            8 => Ok(Self::OpUnsupported),
            9 => Ok(Self::InvalidHandle),
            10 => Ok(Self::NoSuchPath),
            11 => Ok(Self::FileAlreadyExists),
            12 => Ok(Self::WriteProtect),
            13 => Ok(Self::NoMedia),

            // Errors only available with ssh2
            #[cfg(feature = "ssh2")]
            14 => Ok(Self::NoSpaceOnFilesystem),
            #[cfg(feature = "ssh2")]
            15 => Ok(Self::QuotaExceeded),
            #[cfg(feature = "ssh2")]
            16 => Ok(Self::UnknownPrincipal),
            #[cfg(feature = "ssh2")]
            17 => Ok(Self::LockConflict),
            #[cfg(feature = "ssh2")]
            18 => Ok(Self::DirNotEmpty),
            #[cfg(feature = "ssh2")]
            19 => Ok(Self::NotADirectory),
            #[cfg(feature = "ssh2")]
            20 => Ok(Self::InvalidFilename),
            #[cfg(feature = "ssh2")]
            21 => Ok(Self::LinkLoop),

            // Unsupported codes get reflected back
            x => Err(Err(x)),
        }
    }
}

#[cfg(feature = "ssh2")]
impl TryFrom<ssh2::Error> for SftpError {
    type Error = ssh2::Error;

    fn try_from(err: ssh2::Error) -> Result<Self, Self::Error> {
        match err.code() {
            ssh2::ErrorCode::SFTP(x) => match Self::from_error_code(x) {
                Some(err) => Ok(err),
                None => Err(err),
            },
            _ => Err(err),
        }
    }
}

#[cfg(feature = "ssh2")]
impl TryFrom<ssh2::ErrorCode> for SftpError {
    type Error = ssh2::ErrorCode;

    fn try_from(code: ssh2::ErrorCode) -> Result<Self, Self::Error> {
        match code {
            ssh2::ErrorCode::SFTP(x) => match Self::from_error_code(x) {
                Some(err) => Ok(err),
                None => Err(code),
            },
            x => Err(x),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::TryFrom;

    #[test]
    fn try_from_zero_is_success() {
        let result = SftpError::try_from(0);
        assert_eq!(result, Err(Ok(())));
    }

    #[test]
    fn try_from_eof() {
        assert_eq!(SftpError::try_from(1), Ok(SftpError::Eof));
    }

    #[test]
    fn try_from_no_such_file() {
        assert_eq!(SftpError::try_from(2), Ok(SftpError::NoSuchFile));
    }

    #[test]
    fn try_from_permission_denied() {
        assert_eq!(SftpError::try_from(3), Ok(SftpError::PermissionDenied));
    }

    #[test]
    fn try_from_failure() {
        assert_eq!(SftpError::try_from(4), Ok(SftpError::Failure));
    }

    #[test]
    fn try_from_bad_message() {
        assert_eq!(SftpError::try_from(5), Ok(SftpError::BadMessage));
    }

    #[test]
    fn try_from_no_connection() {
        assert_eq!(SftpError::try_from(6), Ok(SftpError::NoConnection));
    }

    #[test]
    fn try_from_connection_lost() {
        assert_eq!(SftpError::try_from(7), Ok(SftpError::ConnectionLost));
    }

    #[test]
    fn try_from_op_unsupported() {
        assert_eq!(SftpError::try_from(8), Ok(SftpError::OpUnsupported));
    }

    #[test]
    fn try_from_invalid_handle() {
        assert_eq!(SftpError::try_from(9), Ok(SftpError::InvalidHandle));
    }

    #[test]
    fn try_from_no_such_path() {
        assert_eq!(SftpError::try_from(10), Ok(SftpError::NoSuchPath));
    }

    #[test]
    fn try_from_file_already_exists() {
        assert_eq!(SftpError::try_from(11), Ok(SftpError::FileAlreadyExists));
    }

    #[test]
    fn try_from_write_protect() {
        assert_eq!(SftpError::try_from(12), Ok(SftpError::WriteProtect));
    }

    #[test]
    fn try_from_no_media() {
        assert_eq!(SftpError::try_from(13), Ok(SftpError::NoMedia));
    }

    #[cfg(feature = "ssh2")]
    #[test]
    fn try_from_no_space_on_filesystem() {
        assert_eq!(SftpError::try_from(14), Ok(SftpError::NoSpaceOnFilesystem));
    }

    #[cfg(feature = "ssh2")]
    #[test]
    fn try_from_quota_exceeded() {
        assert_eq!(SftpError::try_from(15), Ok(SftpError::QuotaExceeded));
    }

    #[cfg(feature = "ssh2")]
    #[test]
    fn try_from_unknown_principal() {
        assert_eq!(SftpError::try_from(16), Ok(SftpError::UnknownPrincipal));
    }

    #[cfg(feature = "ssh2")]
    #[test]
    fn try_from_lock_conflict() {
        assert_eq!(SftpError::try_from(17), Ok(SftpError::LockConflict));
    }

    #[cfg(feature = "ssh2")]
    #[test]
    fn try_from_dir_not_empty() {
        assert_eq!(SftpError::try_from(18), Ok(SftpError::DirNotEmpty));
    }

    #[cfg(feature = "ssh2")]
    #[test]
    fn try_from_not_a_directory() {
        assert_eq!(SftpError::try_from(19), Ok(SftpError::NotADirectory));
    }

    #[cfg(feature = "ssh2")]
    #[test]
    fn try_from_invalid_filename() {
        assert_eq!(SftpError::try_from(20), Ok(SftpError::InvalidFilename));
    }

    #[cfg(feature = "ssh2")]
    #[test]
    fn try_from_link_loop() {
        assert_eq!(SftpError::try_from(21), Ok(SftpError::LinkLoop));
    }

    #[test]
    fn try_from_unknown_code() {
        assert_eq!(SftpError::try_from(999), Err(Err(999)));
    }

    #[test]
    fn try_from_negative_code() {
        assert_eq!(SftpError::try_from(-1), Err(Err(-1)));
    }

    #[test]
    fn from_error_code_valid() {
        assert_eq!(SftpError::from_error_code(1), Some(SftpError::Eof));
        assert_eq!(
            SftpError::from_error_code(3),
            Some(SftpError::PermissionDenied)
        );
    }

    #[test]
    fn from_error_code_zero_is_none() {
        assert_eq!(SftpError::from_error_code(0), None);
    }

    #[test]
    fn from_error_code_unknown_is_none() {
        assert_eq!(SftpError::from_error_code(999), None);
    }

    #[test]
    fn to_error_code_roundtrip() {
        let variants: Vec<(SftpError, i32)> = vec![
            (SftpError::Eof, 1),
            (SftpError::NoSuchFile, 2),
            (SftpError::PermissionDenied, 3),
            (SftpError::Failure, 4),
            (SftpError::BadMessage, 5),
            (SftpError::NoConnection, 6),
            (SftpError::ConnectionLost, 7),
            (SftpError::OpUnsupported, 8),
            (SftpError::InvalidHandle, 9),
            (SftpError::NoSuchPath, 10),
            (SftpError::FileAlreadyExists, 11),
            (SftpError::WriteProtect, 12),
            (SftpError::NoMedia, 13),
        ];
        for (err, code) in variants {
            assert_eq!(err.to_error_code(), code);
            assert_eq!(SftpError::try_from(code), Ok(err));
        }
    }

    #[cfg(feature = "ssh2")]
    #[test]
    fn to_error_code_roundtrip_ssh2_only() {
        let variants: Vec<(SftpError, i32)> = vec![
            (SftpError::NoSpaceOnFilesystem, 14),
            (SftpError::QuotaExceeded, 15),
            (SftpError::UnknownPrincipal, 16),
            (SftpError::LockConflict, 17),
            (SftpError::DirNotEmpty, 18),
            (SftpError::NotADirectory, 19),
            (SftpError::InvalidFilename, 20),
            (SftpError::LinkLoop, 21),
        ];
        for (err, code) in variants {
            assert_eq!(err.to_error_code(), code);
            assert_eq!(SftpError::try_from(code), Ok(err));
        }
    }

    #[test]
    fn error_display_messages() {
        assert_eq!(format!("{}", SftpError::Eof), "End-of-file encountered");
        assert_eq!(format!("{}", SftpError::NoSuchFile), "File doesn't exist");
        assert_eq!(
            format!("{}", SftpError::PermissionDenied),
            "Permission denied"
        );
        assert_eq!(format!("{}", SftpError::Failure), "Generic failure");
        assert_eq!(
            format!("{}", SftpError::BadMessage),
            "Garbage received from server"
        );
        assert_eq!(
            format!("{}", SftpError::NoConnection),
            "No connection has been set up"
        );
        assert_eq!(
            format!("{}", SftpError::ConnectionLost),
            "There was a connection, but we lost it"
        );
        assert_eq!(
            format!("{}", SftpError::OpUnsupported),
            "Operation not supported by the server"
        );
        assert_eq!(
            format!("{}", SftpError::InvalidHandle),
            "Invalid file handle"
        );
        assert_eq!(
            format!("{}", SftpError::NoSuchPath),
            "No such file or directory path exists"
        );
        assert_eq!(
            format!("{}", SftpError::FileAlreadyExists),
            "An attempt to create an already existing file or directory has been made"
        );
        assert_eq!(
            format!("{}", SftpError::WriteProtect),
            "We are trying to write on a write-protected filesystem"
        );
        assert_eq!(
            format!("{}", SftpError::NoMedia),
            "No media in remote drive"
        );
    }

    #[test]
    fn error_clone_and_copy() {
        let err = SftpError::PermissionDenied;
        let cloned = err;
        assert_eq!(err, cloned);
    }

    #[test]
    fn error_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(SftpError::Eof);
        set.insert(SftpError::NoSuchFile);
        set.insert(SftpError::Eof); // duplicate
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn error_debug() {
        let dbg = format!("{:?}", SftpError::PermissionDenied);
        assert!(dbg.contains("PermissionDenied"));
    }

    #[test]
    fn sftp_result_ok() {
        let result: SftpResult<i32> = Ok(42);
        assert_eq!(result.unwrap(), 42);
    }

    #[test]
    fn sftp_result_err() {
        let result: SftpResult<i32> = Err(SftpError::Failure);
        assert_eq!(result.unwrap_err(), SftpError::Failure);
    }
}
