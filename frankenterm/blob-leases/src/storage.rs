use crate::{ContentId, Error, LeaseId};
use std::io::{BufRead, Seek};
use std::sync::{Arc, Mutex};

static STORAGE: Mutex<Option<Arc<dyn BlobStorage + Send + Sync + 'static>>> = Mutex::new(None);

pub trait BufSeekRead: BufRead + Seek {}
pub type BoxedReader = Box<dyn BufSeekRead + Send + Sync>;

/// Implements the actual storage mechanism for blobs
pub trait BlobStorage {
    /// Store data with the provided content_id.
    /// lease_id is provided by the caller to identify this store.
    /// The underlying store is expected to dedup storing data with the same
    /// content_id.
    fn store(&self, content_id: ContentId, data: &[u8], lease_id: LeaseId) -> Result<(), Error>;

    /// Resolve the data associated with content_id.
    /// If found, establish a lease with the given lease_id.
    /// If not found, returns Err(Error::ContentNotFound)
    fn lease_by_content(&self, content_id: ContentId, lease_id: LeaseId) -> Result<(), Error>;

    /// Retrieves the data identified by content_id.
    /// lease_id is provided in order to advise the storage system
    /// which lease fetched it, so that it can choose to record that
    /// information to track the liveness of a lease
    fn get_data(&self, content_id: ContentId, lease_id: LeaseId) -> Result<Vec<u8>, Error>;

    /// Retrieves the data identified by content_id as a readable+seekable
    /// buffered handle.
    ///
    /// lease_id is provided in order to advise the storage system
    /// which lease fetched it, so that it can choose to record that
    /// information to track the liveness of a lease.
    ///
    /// The returned handle serves to extend the lifetime of the lease.
    fn get_reader(&self, content_id: ContentId, lease_id: LeaseId) -> Result<BoxedReader, Error>;

    /// Advises the storage manager that a particular lease has been dropped.
    fn advise_lease_dropped(&self, lease_id: LeaseId, content_id: ContentId) -> Result<(), Error>;
    /// Advises the storage manager that a given process id is now, or
    /// continues to be, alive and a valid consumer of the store.
    fn advise_of_pid(&self, pid: u32) -> Result<(), Error>;

    /// Advises the storage manager that a given process id is, or will
    /// very shortly, terminate and will cease to be a valid consumer
    /// of the store.
    /// It may choose to do something to invalidate all leases with
    /// a corresponding pid.
    fn advise_pid_terminated(&self, pid: u32) -> Result<(), Error>;
}

pub fn register_storage(
    storage: Arc<dyn BlobStorage + Send + Sync + 'static>,
) -> Result<(), Error> {
    STORAGE.lock().unwrap().replace(storage);
    Ok(())
}

pub fn get_storage() -> Result<Arc<dyn BlobStorage + Send + Sync + 'static>, Error> {
    STORAGE
        .lock()
        .unwrap()
        .as_ref()
        .map(|s| s.clone())
        .ok_or_else(|| Error::StorageNotInit)
}

pub fn clear_storage() {
    STORAGE.lock().unwrap().take();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::Mutex as StdMutex;

    // Serialize tests that touch the global STORAGE to avoid interference
    static TEST_LOCK: StdMutex<()> = StdMutex::new(());

    /// Minimal in-memory storage for testing the register/get/clear cycle
    struct InMemoryStorage {
        data: StdMutex<std::collections::HashMap<ContentId, Vec<u8>>>,
    }

    impl InMemoryStorage {
        fn new() -> Self {
            Self {
                data: StdMutex::new(std::collections::HashMap::new()),
            }
        }
    }

    struct InMemoryReader(Cursor<Vec<u8>>);
    impl BufSeekRead for InMemoryReader {}
    impl std::io::Read for InMemoryReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.0.read(buf)
        }
    }
    impl std::io::BufRead for InMemoryReader {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            self.0.fill_buf()
        }
        fn consume(&mut self, amt: usize) {
            self.0.consume(amt);
        }
    }
    impl std::io::Seek for InMemoryReader {
        fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
            self.0.seek(pos)
        }
    }

    impl BlobStorage for InMemoryStorage {
        fn store(
            &self,
            content_id: ContentId,
            data: &[u8],
            _lease_id: LeaseId,
        ) -> Result<(), Error> {
            self.data.lock().unwrap().insert(content_id, data.to_vec());
            Ok(())
        }
        fn lease_by_content(&self, content_id: ContentId, _lease_id: LeaseId) -> Result<(), Error> {
            if self.data.lock().unwrap().contains_key(&content_id) {
                Ok(())
            } else {
                Err(Error::ContentNotFound(content_id))
            }
        }
        fn get_data(&self, content_id: ContentId, _lease_id: LeaseId) -> Result<Vec<u8>, Error> {
            self.data
                .lock()
                .unwrap()
                .get(&content_id)
                .cloned()
                .ok_or(Error::ContentNotFound(content_id))
        }
        fn get_reader(
            &self,
            content_id: ContentId,
            lease_id: LeaseId,
        ) -> Result<BoxedReader, Error> {
            let data = self.get_data(content_id, lease_id)?;
            Ok(Box::new(InMemoryReader(Cursor::new(data))))
        }
        fn advise_lease_dropped(
            &self,
            _lease_id: LeaseId,
            _content_id: ContentId,
        ) -> Result<(), Error> {
            Ok(())
        }
        fn advise_of_pid(&self, _pid: u32) -> Result<(), Error> {
            Ok(())
        }
        fn advise_pid_terminated(&self, _pid: u32) -> Result<(), Error> {
            Ok(())
        }
    }

    #[test]
    fn get_storage_without_registration_returns_error() {
        let _lock = TEST_LOCK.lock().unwrap();
        clear_storage();
        let result = get_storage();
        assert!(result.is_err());
        assert!(result
            .err()
            .unwrap()
            .to_string()
            .contains("not been initialized"));
    }

    #[test]
    fn register_then_get_succeeds() {
        let _lock = TEST_LOCK.lock().unwrap();
        clear_storage();
        let storage = Arc::new(InMemoryStorage::new());
        register_storage(storage).unwrap();
        assert!(get_storage().is_ok());
        clear_storage();
    }

    #[test]
    fn clear_storage_makes_get_fail() {
        let _lock = TEST_LOCK.lock().unwrap();
        let storage = Arc::new(InMemoryStorage::new());
        register_storage(storage).unwrap();
        clear_storage();
        assert!(get_storage().is_err());
    }

    #[test]
    fn register_overwrites_previous_storage() {
        let _lock = TEST_LOCK.lock().unwrap();
        clear_storage();
        let s1 = Arc::new(InMemoryStorage::new());
        let s2 = Arc::new(InMemoryStorage::new());
        register_storage(s1).unwrap();
        register_storage(s2).unwrap();
        assert!(get_storage().is_ok());
        clear_storage();
    }

    #[test]
    fn in_memory_store_and_get_roundtrip() {
        let _lock = TEST_LOCK.lock().unwrap();
        clear_storage();

        let storage = Arc::new(InMemoryStorage::new());
        register_storage(storage).unwrap();

        let s = get_storage().unwrap();
        let content_id = ContentId::for_bytes(b"hello");
        let lease_id = LeaseId::new();
        s.store(content_id, b"hello", lease_id).unwrap();

        let data = s.get_data(content_id, lease_id).unwrap();
        assert_eq!(data, b"hello");
        clear_storage();
    }

    #[test]
    fn in_memory_get_nonexistent_returns_not_found() {
        let _lock = TEST_LOCK.lock().unwrap();
        clear_storage();

        let storage = Arc::new(InMemoryStorage::new());
        register_storage(storage).unwrap();

        let s = get_storage().unwrap();
        let content_id = ContentId::for_bytes(b"missing");
        let lease_id = LeaseId::new();
        let result = s.get_data(content_id, lease_id);
        assert!(result.is_err());
        clear_storage();
    }

    #[test]
    fn in_memory_reader_returns_stored_data() {
        let _lock = TEST_LOCK.lock().unwrap();
        clear_storage();

        let storage = Arc::new(InMemoryStorage::new());
        register_storage(storage).unwrap();

        let s = get_storage().unwrap();
        let content_id = ContentId::for_bytes(b"reader test");
        let lease_id = LeaseId::new();
        s.store(content_id, b"reader test", lease_id).unwrap();

        let mut reader = s.get_reader(content_id, lease_id).unwrap();
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut reader, &mut buf).unwrap();
        assert_eq!(buf, b"reader test");
        clear_storage();
    }
}
