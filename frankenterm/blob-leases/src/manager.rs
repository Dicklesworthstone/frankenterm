use crate::{get_storage, BlobLease, ContentId, Error, LeaseId};

pub struct BlobManager {}

impl BlobManager {
    /// Store data into the store, de-duplicating it and returning
    /// a BlobLease that can be used to reference and access it.
    pub fn store(data: &[u8]) -> Result<BlobLease, Error> {
        let storage = get_storage()?;

        let lease_id = LeaseId::new();
        let content_id = ContentId::for_bytes(data);

        storage.store(content_id, data, lease_id)?;

        Ok(BlobLease::make_lease(content_id, lease_id))
    }

    /// Attempt to resolve by content id
    pub fn get_by_content_id(content_id: ContentId) -> Result<BlobLease, Error> {
        let storage = get_storage()?;

        let lease_id = LeaseId::new();
        storage.lease_by_content(content_id, lease_id)?;

        Ok(BlobLease::make_lease(content_id, lease_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{
        clear_storage, register_storage, BlobStorage, BoxedReader, BufSeekRead, TEST_LOCK,
    };
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
    fn store_without_storage_fails() {
        let _lock = TEST_LOCK.lock().unwrap();
        clear_storage();
        let result = BlobManager::store(b"data");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("not been initialized"));
    }

    #[test]
    fn store_returns_valid_lease() {
        let _lock = TEST_LOCK.lock().unwrap();
        let _s = setup_storage();
        let lease = BlobManager::store(b"hello").unwrap();
        let expected_id = ContentId::for_bytes(b"hello");
        assert_eq!(lease.content_id(), expected_id);
        clear_storage();
    }

    #[test]
    fn store_and_get_data_roundtrip() {
        let _lock = TEST_LOCK.lock().unwrap();
        let _s = setup_storage();
        let lease = BlobManager::store(b"roundtrip data").unwrap();
        let data = lease.get_data().unwrap();
        assert_eq!(data, b"roundtrip data");
        clear_storage();
    }

    #[test]
    fn store_empty_data() {
        let _lock = TEST_LOCK.lock().unwrap();
        let _s = setup_storage();
        let lease = BlobManager::store(b"").unwrap();
        let data = lease.get_data().unwrap();
        assert!(data.is_empty());
        clear_storage();
    }

    #[test]
    fn store_large_data() {
        let _lock = TEST_LOCK.lock().unwrap();
        let _s = setup_storage();
        let big = vec![0xABu8; 100_000];
        let lease = BlobManager::store(&big).unwrap();
        let data = lease.get_data().unwrap();
        assert_eq!(data.len(), 100_000);
        assert!(data.iter().all(|&b| b == 0xAB));
        clear_storage();
    }

    #[test]
    fn get_by_content_id_succeeds_for_stored() {
        let _lock = TEST_LOCK.lock().unwrap();
        let _s = setup_storage();
        let lease1 = BlobManager::store(b"find me").unwrap();
        let cid = lease1.content_id();
        let lease2 = BlobManager::get_by_content_id(cid).unwrap();
        assert_eq!(lease2.content_id(), cid);
        let data = lease2.get_data().unwrap();
        assert_eq!(data, b"find me");
        clear_storage();
    }

    #[test]
    fn get_by_content_id_fails_for_missing() {
        let _lock = TEST_LOCK.lock().unwrap();
        let _s = setup_storage();
        let cid = ContentId::for_bytes(b"not stored");
        let result = BlobManager::get_by_content_id(cid);
        assert!(result.is_err());
        clear_storage();
    }

    #[test]
    fn get_by_content_id_without_storage_fails() {
        let _lock = TEST_LOCK.lock().unwrap();
        clear_storage();
        let cid = ContentId::for_bytes(b"anything");
        let result = BlobManager::get_by_content_id(cid);
        assert!(result.is_err());
    }

    #[test]
    fn store_same_data_twice_deduplicates() {
        let _lock = TEST_LOCK.lock().unwrap();
        let _s = setup_storage();
        let lease1 = BlobManager::store(b"same").unwrap();
        let lease2 = BlobManager::store(b"same").unwrap();
        assert_eq!(lease1.content_id(), lease2.content_id());
        clear_storage();
    }

    #[test]
    fn store_different_data_produces_different_ids() {
        let _lock = TEST_LOCK.lock().unwrap();
        let _s = setup_storage();
        let lease1 = BlobManager::store(b"alpha").unwrap();
        let lease2 = BlobManager::store(b"beta").unwrap();
        assert_ne!(lease1.content_id(), lease2.content_id());
        clear_storage();
    }

    #[test]
    fn get_by_content_id_data_matches_original() {
        let _lock = TEST_LOCK.lock().unwrap();
        let _s = setup_storage();
        BlobManager::store(b"verify data").unwrap();
        let cid = ContentId::for_bytes(b"verify data");
        let lease = BlobManager::get_by_content_id(cid).unwrap();
        assert_eq!(lease.get_data().unwrap(), b"verify data");
        clear_storage();
    }

    #[test]
    fn store_binary_data() {
        let _lock = TEST_LOCK.lock().unwrap();
        let _s = setup_storage();
        let data: Vec<u8> = (0..=255).collect();
        let lease = BlobManager::store(&data).unwrap();
        assert_eq!(lease.get_data().unwrap(), data);
        clear_storage();
    }

    #[test]
    fn store_multiple_items_independently() {
        let _lock = TEST_LOCK.lock().unwrap();
        let _s = setup_storage();
        let l1 = BlobManager::store(b"first").unwrap();
        let l2 = BlobManager::store(b"second").unwrap();
        let l3 = BlobManager::store(b"third").unwrap();
        assert_eq!(l1.get_data().unwrap(), b"first");
        assert_eq!(l2.get_data().unwrap(), b"second");
        assert_eq!(l3.get_data().unwrap(), b"third");
        clear_storage();
    }

    #[test]
    fn get_by_content_id_after_multiple_stores() {
        let _lock = TEST_LOCK.lock().unwrap();
        let _s = setup_storage();
        BlobManager::store(b"aaa").unwrap();
        BlobManager::store(b"bbb").unwrap();
        BlobManager::store(b"ccc").unwrap();
        let cid = ContentId::for_bytes(b"bbb");
        let lease = BlobManager::get_by_content_id(cid).unwrap();
        assert_eq!(lease.get_data().unwrap(), b"bbb");
        clear_storage();
    }

    #[test]
    fn store_returns_unique_lease_ids_for_same_content() {
        let _lock = TEST_LOCK.lock().unwrap();
        let _s = setup_storage();
        let l1 = BlobManager::store(b"dup").unwrap();
        let l2 = BlobManager::store(b"dup").unwrap();
        // Same content id, but different lease objects (different internal lease_id)
        assert_eq!(l1.content_id(), l2.content_id());
        // Both should be able to retrieve data independently
        assert_eq!(l1.get_data().unwrap(), b"dup");
        assert_eq!(l2.get_data().unwrap(), b"dup");
        clear_storage();
    }

    #[test]
    fn store_single_byte() {
        let _lock = TEST_LOCK.lock().unwrap();
        let _s = setup_storage();
        let lease = BlobManager::store(&[0xFF]).unwrap();
        assert_eq!(lease.get_data().unwrap(), vec![0xFF]);
        clear_storage();
    }

    #[test]
    fn get_by_content_id_returns_correct_data_after_overwrite() {
        let _lock = TEST_LOCK.lock().unwrap();
        let _s = setup_storage();
        // Store same content twice; second store overwrites in InMemoryStorage
        let _l1 = BlobManager::store(b"persistent").unwrap();
        let _l2 = BlobManager::store(b"persistent").unwrap();
        let cid = ContentId::for_bytes(b"persistent");
        let lease = BlobManager::get_by_content_id(cid).unwrap();
        assert_eq!(lease.get_data().unwrap(), b"persistent");
        clear_storage();
    }
}
