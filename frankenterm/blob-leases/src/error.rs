use crate::ContentId;
use std::path::PathBuf;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Lease Expired, data is no longer accessible")]
    LeaseExpired,

    #[error("Content with id {0} not found")]
    ContentNotFound(ContentId),

    #[error("Io error in BlobLease: {0}")]
    Io(#[from] std::io::Error),

    #[error("Storage has already been initialized")]
    AlreadyInitializedStorage,

    #[error("Storage has not been initialized")]
    StorageNotInit,

    #[error("Storage location {0} may be corrupt: {1}")]
    StorageDirIoError(PathBuf, std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_expired_display() {
        let err = Error::LeaseExpired;
        assert_eq!(
            err.to_string(),
            "Lease Expired, data is no longer accessible"
        );
    }

    #[test]
    fn content_not_found_includes_id() {
        let id = ContentId::for_bytes(b"missing");
        let err = Error::ContentNotFound(id);
        let msg = err.to_string();
        assert!(msg.contains("not found"), "got: {msg}");
        assert!(msg.contains("sha256-"), "got: {msg}");
    }

    #[test]
    fn io_error_from_conversion() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file gone");
        let err: Error = io_err.into();
        assert!(err.to_string().contains("file gone"));
    }

    #[test]
    fn storage_not_init_display() {
        let err = Error::StorageNotInit;
        assert_eq!(err.to_string(), "Storage has not been initialized");
    }

    #[test]
    fn already_initialized_display() {
        let err = Error::AlreadyInitializedStorage;
        assert!(err.to_string().contains("already been initialized"));
    }

    #[test]
    fn storage_dir_io_error_includes_path() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let err = Error::StorageDirIoError(PathBuf::from("/tmp/blobs"), io_err);
        let msg = err.to_string();
        assert!(msg.contains("/tmp/blobs"), "got: {msg}");
        assert!(msg.contains("denied"), "got: {msg}");
    }

    #[test]
    fn lease_expired_debug_output() {
        let err = Error::LeaseExpired;
        let debug = format!("{err:?}");
        assert!(debug.contains("LeaseExpired"));
    }

    #[test]
    fn io_error_source_is_some() {
        use std::error::Error as StdError;
        let io_err = std::io::Error::new(std::io::ErrorKind::Other, "oops");
        let err: Error = io_err.into();
        assert!(err.source().is_some());
    }

    #[test]
    fn content_not_found_debug_includes_hash() {
        let id = ContentId::for_bytes(b"gone");
        let err = Error::ContentNotFound(id);
        let debug = format!("{err:?}");
        assert!(debug.contains("ContentNotFound"));
        assert!(debug.contains("sha256-"));
    }

    #[test]
    fn storage_dir_io_error_debug_output() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        let err = Error::StorageDirIoError(PathBuf::from("/data"), io_err);
        let debug = format!("{err:?}");
        assert!(debug.contains("StorageDirIoError"));
    }

    #[test]
    fn storage_not_init_source_is_none() {
        use std::error::Error as StdError;
        let err = Error::StorageNotInit;
        assert!(err.source().is_none());
    }

    #[test]
    fn lease_expired_source_is_none() {
        use std::error::Error as StdError;
        let err = Error::LeaseExpired;
        assert!(err.source().is_none());
    }

    #[test]
    fn already_initialized_source_is_none() {
        use std::error::Error as StdError;
        let err = Error::AlreadyInitializedStorage;
        assert!(err.source().is_none());
    }

    #[test]
    fn storage_dir_io_error_source_is_none() {
        use std::error::Error as StdError;
        let io_err = std::io::Error::new(std::io::ErrorKind::Other, "disk fail");
        let err = Error::StorageDirIoError(PathBuf::from("/store"), io_err);
        // No #[source] attribute on tuple variant, so source is None
        assert!(err.source().is_none());
    }

    #[test]
    fn io_error_preserves_error_kind() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "no access");
        let err: Error = io_err.into();
        let msg = err.to_string();
        assert!(msg.contains("no access"), "got: {msg}");
    }

    #[test]
    fn already_initialized_debug_output() {
        let err = Error::AlreadyInitializedStorage;
        let debug = format!("{err:?}");
        assert!(debug.contains("AlreadyInitializedStorage"));
    }

    #[test]
    fn content_not_found_display_starts_with_content() {
        let id = ContentId::for_bytes(b"test");
        let err = Error::ContentNotFound(id);
        assert!(err.to_string().starts_with("Content with id"));
    }
}
