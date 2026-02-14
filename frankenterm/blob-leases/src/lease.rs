use crate::{get_storage, BoxedReader, ContentId, Error, LeaseId};
use std::sync::Arc;

/// A lease represents a handle to data in the store.
/// The lease will help to keep the data alive in the store.
/// Depending on the policy configured for the store, it
/// may guarantee to keep the data intact for its lifetime,
/// or in some cases, it the store is being thrashed and at
/// capacity, it may have been evicted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobLease {
    inner: Arc<LeaseInner>,
}

#[derive(Debug, PartialEq, Eq)]
struct LeaseInner {
    pub content_id: ContentId,
    pub lease_id: LeaseId,
}

impl BlobLease {
    pub(crate) fn make_lease(content_id: ContentId, lease_id: LeaseId) -> Self {
        Self {
            inner: Arc::new(LeaseInner {
                content_id,
                lease_id,
            }),
        }
    }

    /// Returns a copy of the data, owned by the caller
    pub fn get_data(&self) -> Result<Vec<u8>, Error> {
        let storage = get_storage()?;
        storage.get_data(self.inner.content_id, self.inner.lease_id)
    }

    /// Returns a reader that can be used to stream/seek into
    /// the data
    pub fn get_reader(&self) -> Result<BoxedReader, Error> {
        let storage = get_storage()?;
        storage.get_reader(self.inner.content_id, self.inner.lease_id)
    }

    pub fn content_id(&self) -> ContentId {
        self.inner.content_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{clear_storage, register_storage, BlobStorage, BoxedReader, BufSeekRead, TEST_LOCK};
    use crate::{BlobManager, LeaseId};
    use std::io::Cursor;
    use std::sync::{Arc, Mutex};

    struct InMemoryStorage {
        data: Mutex<std::collections::HashMap<ContentId, Vec<u8>>>,
    }

    impl InMemoryStorage {
        fn new() -> Self {
            Self {
                data: Mutex::new(std::collections::HashMap::new()),
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

    fn setup_storage() -> Arc<InMemoryStorage> {
        let s = Arc::new(InMemoryStorage::new());
        clear_storage();
        register_storage(s.clone()).unwrap();
        s
    }

    #[test]
    fn blob_lease_get_data() {
        let _lock = TEST_LOCK.lock().unwrap();
        let _s = setup_storage();
        let lease = BlobManager::store(b"lease data").unwrap();
        let data = lease.get_data().unwrap();
        assert_eq!(data, b"lease data");
        clear_storage();
    }

    #[test]
    fn blob_lease_get_reader() {
        let _lock = TEST_LOCK.lock().unwrap();
        let _s = setup_storage();
        let lease = BlobManager::store(b"reader data").unwrap();
        let mut reader = lease.get_reader().unwrap();
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut reader, &mut buf).unwrap();
        assert_eq!(buf, b"reader data");
        clear_storage();
    }

    #[test]
    fn blob_lease_content_id_matches() {
        let _lock = TEST_LOCK.lock().unwrap();
        let _s = setup_storage();
        let lease = BlobManager::store(b"check id").unwrap();
        let expected = ContentId::for_bytes(b"check id");
        assert_eq!(lease.content_id(), expected);
        clear_storage();
    }

    #[test]
    fn blob_lease_clone_shares_content_id() {
        let _lock = TEST_LOCK.lock().unwrap();
        let _s = setup_storage();
        let lease = BlobManager::store(b"cloneable").unwrap();
        let cloned = lease.clone();
        assert_eq!(lease.content_id(), cloned.content_id());
        clear_storage();
    }

    #[test]
    fn blob_lease_clone_equality() {
        let _lock = TEST_LOCK.lock().unwrap();
        let _s = setup_storage();
        let lease = BlobManager::store(b"equal").unwrap();
        let cloned = lease.clone();
        assert_eq!(lease, cloned);
        clear_storage();
    }

    #[test]
    fn blob_lease_is_debug() {
        let _lock = TEST_LOCK.lock().unwrap();
        let _s = setup_storage();
        let lease = BlobManager::store(b"debug").unwrap();
        let debug = format!("{lease:?}");
        assert!(debug.contains("BlobLease"));
        clear_storage();
    }

    #[test]
    fn blob_lease_get_data_without_storage_fails() {
        let _lock = TEST_LOCK.lock().unwrap();
        let _s = setup_storage();
        let lease = BlobManager::store(b"temp").unwrap();
        clear_storage();
        let result = lease.get_data();
        assert!(result.is_err());
    }

    #[test]
    fn blob_lease_drop_does_not_panic() {
        let _lock = TEST_LOCK.lock().unwrap();
        let _s = setup_storage();
        {
            let _lease = BlobManager::store(b"drop me").unwrap();
            // lease drops here
        }
        clear_storage();
    }

    #[test]
    fn blob_lease_drop_without_storage_does_not_panic() {
        let _lock = TEST_LOCK.lock().unwrap();
        let _s = setup_storage();
        let lease = BlobManager::store(b"orphan").unwrap();
        clear_storage();
        drop(lease); // Should not panic even though storage is gone
    }

    #[test]
    fn blob_lease_reader_seek_works() {
        let _lock = TEST_LOCK.lock().unwrap();
        let _s = setup_storage();
        let lease = BlobManager::store(b"seekable content").unwrap();
        let mut reader = lease.get_reader().unwrap();
        std::io::Seek::seek(&mut reader, std::io::SeekFrom::Start(9)).unwrap();
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut reader, &mut buf).unwrap();
        assert_eq!(buf, b"content");
        clear_storage();
    }

    #[test]
    fn two_leases_same_data_have_same_content_id() {
        let _lock = TEST_LOCK.lock().unwrap();
        let _s = setup_storage();
        let l1 = BlobManager::store(b"shared").unwrap();
        let l2 = BlobManager::store(b"shared").unwrap();
        assert_eq!(l1.content_id(), l2.content_id());
        clear_storage();
    }

    #[test]
    fn blob_lease_get_reader_without_storage_fails() {
        let _lock = TEST_LOCK.lock().unwrap();
        let _s = setup_storage();
        let lease = BlobManager::store(b"temp reader").unwrap();
        clear_storage();
        assert!(lease.get_reader().is_err());
    }

    #[test]
    fn blob_lease_reader_partial_read() {
        let _lock = TEST_LOCK.lock().unwrap();
        let _s = setup_storage();
        let lease = BlobManager::store(b"partial read test").unwrap();
        let mut reader = lease.get_reader().unwrap();
        let mut buf = [0u8; 7];
        std::io::Read::read_exact(&mut reader, &mut buf).unwrap();
        assert_eq!(&buf, b"partial");
        clear_storage();
    }

    #[test]
    fn blob_lease_clone_can_get_data_independently() {
        let _lock = TEST_LOCK.lock().unwrap();
        let _s = setup_storage();
        let lease = BlobManager::store(b"clone data").unwrap();
        let cloned = lease.clone();
        assert_eq!(cloned.get_data().unwrap(), b"clone data");
        assert_eq!(lease.get_data().unwrap(), b"clone data");
        clear_storage();
    }

    #[test]
    fn blob_lease_different_data_not_equal() {
        let _lock = TEST_LOCK.lock().unwrap();
        let _s = setup_storage();
        let l1 = BlobManager::store(b"alpha").unwrap();
        let l2 = BlobManager::store(b"beta").unwrap();
        assert_ne!(l1.content_id(), l2.content_id());
        assert_ne!(l1, l2);
        clear_storage();
    }

    #[test]
    fn blob_lease_empty_data_roundtrip() {
        let _lock = TEST_LOCK.lock().unwrap();
        let _s = setup_storage();
        let lease = BlobManager::store(b"").unwrap();
        let mut reader = lease.get_reader().unwrap();
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut reader, &mut buf).unwrap();
        assert!(buf.is_empty());
        clear_storage();
    }
}

impl Drop for LeaseInner {
    fn drop(&mut self) {
        if let Ok(storage) = get_storage() {
            storage
                .advise_lease_dropped(self.lease_id, self.content_id)
                .ok();
        }
    }
}

/// Serialize a lease as the corresponding data bytes.
/// This can fail during serialization if the lease is
/// stale, but not during deserialization, as deserialiation
/// will store the data implicitly.
#[cfg(feature = "serde")]
pub mod lease_bytes {
    use super::*;
    use crate::BlobManager;
    use serde::{de, ser, Deserialize, Serialize};

    /// Serialize a lease as its bytes
    pub fn serialize<S>(lease: &BlobLease, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: ser::Serializer,
    {
        let data = lease
            .get_data()
            .map_err(|err| ser::Error::custom(format!("{err:#}")))?;
        data.serialize(serializer)
    }

    /// Deserialize a lease from bytes.
    pub fn deserialize<'de, D>(d: D) -> Result<BlobLease, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        let data = <Vec<u8> as Deserialize>::deserialize(d)?;

        BlobManager::store(&data).map_err(|err| de::Error::custom(format!("{err:#}")))
    }
}

/// Serialize a lease to/from its content id.
/// This can fail in either direction if the lease is stale
/// during serialization, or if the data for that content id
/// is not available during deserialization.
#[cfg(feature = "serde")]
pub mod lease_content_id {

    use super::*;
    use crate::BlobManager;
    use serde::{de, ser, Deserialize, Serialize};

    /// Serialize a lease as its content id
    pub fn serialize<S>(lease: &BlobLease, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: ser::Serializer,
    {
        lease.inner.content_id.serialize(serializer)
    }

    /// Deserialize a lease from a content id.
    /// Will fail unless the content id is already available
    /// to the local storage manager
    pub fn deserialize<'de, D>(d: D) -> Result<BlobLease, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        let content_id = <ContentId as Deserialize>::deserialize(d)?;
        BlobManager::get_by_content_id(content_id)
            .map_err(|err| de::Error::custom(format!("{err:#}")))
    }
}
