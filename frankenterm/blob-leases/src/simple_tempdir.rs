#![cfg(feature = "simple_tempdir")]

use crate::{BlobStorage, BoxedReader, BufSeekRead, ContentId, Error, LeaseId};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tempfile::TempDir;

pub struct SimpleTempDir {
    root: TempDir,
    refs: Mutex<HashMap<ContentId, usize>>,
}

impl SimpleTempDir {
    pub fn new() -> Result<Self, Error> {
        let root = tempfile::Builder::new()
            .prefix("wezterm-blob-lease-")
            .rand_bytes(8)
            .tempdir()?;
        Ok(Self {
            root,
            refs: Mutex::new(HashMap::new()),
        })
    }

    pub fn new_in<P: AsRef<Path>>(path: P) -> Result<Self, Error> {
        let path = path.as_ref();
        std::fs::create_dir_all(path)?;
        let root = tempfile::Builder::new()
            .prefix("wezterm-blob-lease-")
            .rand_bytes(8)
            .tempdir_in(path)?;
        Ok(Self {
            root,
            refs: Mutex::new(HashMap::new()),
        })
    }

    fn path_for_content(&self, content_id: ContentId) -> Result<PathBuf, Error> {
        let path = self.root.path().join(format!("{content_id}"));
        std::fs::create_dir_all(path.parent().unwrap())
            .map_err(|err| Error::StorageDirIoError(path.clone(), err))?;
        Ok(path)
    }

    fn add_ref(&self, content_id: ContentId) {
        *self.refs.lock().unwrap().entry(content_id).or_insert(0) += 1;
    }

    fn del_ref(&self, content_id: ContentId) {
        let mut refs = self.refs.lock().unwrap();
        match refs.get_mut(&content_id) {
            Some(count) if *count == 1 => {
                if let Ok(path) = self.path_for_content(content_id) {
                    if let Err(err) = std::fs::remove_file(&path) {
                        eprintln!("Failed to remove {}: {err:#}", path.display());
                    }
                }
                *count = 0;
            }
            Some(count) => {
                *count -= 1;
            }
            None => {
                // Shouldn't really happen...
            }
        }
    }
}

impl BlobStorage for SimpleTempDir {
    fn store(&self, content_id: ContentId, data: &[u8], _lease_id: LeaseId) -> Result<(), Error> {
        let mut refs = self.refs.lock().unwrap();

        let path = self.path_for_content(content_id)?;
        let mut file = tempfile::Builder::new()
            .prefix("new-")
            .rand_bytes(5)
            .tempfile_in(&self.root.path())?;

        file.write_all(data)?;
        file.persist(&path)
            .map_err(|persist_err| persist_err.error)?;

        *refs.entry(content_id).or_insert(0) += 1;

        Ok(())
    }

    fn lease_by_content(&self, content_id: ContentId, _lease_id: LeaseId) -> Result<(), Error> {
        let path = self.path_for_content(content_id)?;
        if path.exists() {
            self.add_ref(content_id);
            Ok(())
        } else {
            Err(Error::ContentNotFound(content_id))
        }
    }

    fn get_data(&self, content_id: ContentId, _lease_id: LeaseId) -> Result<Vec<u8>, Error> {
        let _refs = self.refs.lock().unwrap();

        let path = self.path_for_content(content_id)?;
        Ok(std::fs::read(&path).map_err(|err| Error::StorageDirIoError(path, err))?)
    }

    fn get_reader(&self, content_id: ContentId, lease_id: LeaseId) -> Result<BoxedReader, Error> {
        struct Reader {
            file: BufReader<File>,
            content_id: ContentId,
            lease_id: LeaseId,
        }

        impl BufSeekRead for Reader {}

        impl std::io::BufRead for Reader {
            fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
                self.file.fill_buf()
            }
            fn consume(&mut self, amount: usize) {
                self.file.consume(amount)
            }
        }

        impl std::io::Read for Reader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.file.read(buf)
            }
        }

        impl std::io::Seek for Reader {
            fn seek(&mut self, whence: std::io::SeekFrom) -> std::io::Result<u64> {
                self.file.seek(whence)
            }
        }

        impl Drop for Reader {
            fn drop(&mut self) {
                if let Ok(s) = crate::get_storage() {
                    s.advise_lease_dropped(self.lease_id, self.content_id).ok();
                }
            }
        }

        let path = self.path_for_content(content_id)?;
        let file = BufReader::new(std::fs::File::open(&path)?);
        self.add_ref(content_id);

        Ok(Box::new(Reader {
            file,
            content_id,
            lease_id,
        }))
    }

    fn advise_lease_dropped(&self, _lease_id: LeaseId, content_id: ContentId) -> Result<(), Error> {
        self.del_ref(content_id);
        Ok(())
    }

    fn advise_of_pid(&self, _pid: u32) -> Result<(), Error> {
        Ok(())
    }

    fn advise_pid_terminated(&self, _pid: u32) -> Result<(), Error> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_creates_temp_directory() {
        let store = SimpleTempDir::new().unwrap();
        assert!(store.root.path().exists());
    }

    #[test]
    fn new_in_creates_under_specified_path() {
        let parent = tempfile::tempdir().unwrap();
        let store = SimpleTempDir::new_in(parent.path()).unwrap();
        assert!(store.root.path().starts_with(parent.path()));
    }

    #[test]
    fn store_and_get_roundtrip() {
        let store = SimpleTempDir::new().unwrap();
        let content_id = ContentId::for_bytes(b"hello");
        let lease_id = LeaseId::new();
        store.store(content_id, b"hello", lease_id).unwrap();

        let data = store.get_data(content_id, lease_id).unwrap();
        assert_eq!(data, b"hello");
    }

    #[test]
    fn store_empty_data() {
        let store = SimpleTempDir::new().unwrap();
        let content_id = ContentId::for_bytes(b"");
        let lease_id = LeaseId::new();
        store.store(content_id, b"", lease_id).unwrap();

        let data = store.get_data(content_id, lease_id).unwrap();
        assert!(data.is_empty());
    }

    #[test]
    fn store_large_data() {
        let store = SimpleTempDir::new().unwrap();
        let big = vec![0xABu8; 1_000_000];
        let content_id = ContentId::for_bytes(&big);
        let lease_id = LeaseId::new();
        store.store(content_id, &big, lease_id).unwrap();

        let data = store.get_data(content_id, lease_id).unwrap();
        assert_eq!(data.len(), 1_000_000);
        assert_eq!(data, big);
    }

    #[test]
    fn dedup_same_content_id() {
        let store = SimpleTempDir::new().unwrap();
        let content_id = ContentId::for_bytes(b"dedup");
        let lease1 = LeaseId::new();
        let lease2 = LeaseId::new();

        store.store(content_id, b"dedup", lease1).unwrap();
        store.store(content_id, b"dedup", lease2).unwrap();

        let data = store.get_data(content_id, lease1).unwrap();
        assert_eq!(data, b"dedup");
    }

    #[test]
    fn get_nonexistent_content_fails() {
        let store = SimpleTempDir::new().unwrap();
        let content_id = ContentId::for_bytes(b"missing");
        let lease_id = LeaseId::new();

        let result = store.get_data(content_id, lease_id);
        assert!(result.is_err());
    }

    #[test]
    fn lease_by_content_succeeds_for_stored_data() {
        let store = SimpleTempDir::new().unwrap();
        let content_id = ContentId::for_bytes(b"leaseable");
        let lease_id = LeaseId::new();
        store.store(content_id, b"leaseable", lease_id).unwrap();

        let new_lease = LeaseId::new();
        store.lease_by_content(content_id, new_lease).unwrap();
    }

    #[test]
    fn lease_by_content_fails_for_missing_data() {
        let store = SimpleTempDir::new().unwrap();
        let content_id = ContentId::for_bytes(b"not stored");
        let lease_id = LeaseId::new();

        let result = store.lease_by_content(content_id, lease_id);
        assert!(result.is_err());
    }

    #[test]
    fn get_reader_returns_correct_data() {
        let store = SimpleTempDir::new().unwrap();
        let content_id = ContentId::for_bytes(b"reader data");
        let lease_id = LeaseId::new();
        store.store(content_id, b"reader data", lease_id).unwrap();

        let mut reader = store.get_reader(content_id, lease_id).unwrap();
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut reader, &mut buf).unwrap();
        assert_eq!(buf, b"reader data");
    }

    #[test]
    fn get_reader_supports_seek() {
        let store = SimpleTempDir::new().unwrap();
        let content_id = ContentId::for_bytes(b"seekable");
        let lease_id = LeaseId::new();
        store.store(content_id, b"seekable", lease_id).unwrap();

        let mut reader = store.get_reader(content_id, lease_id).unwrap();
        // Seek to position 4
        use std::io::{Read, Seek, SeekFrom};
        reader.seek(SeekFrom::Start(4)).unwrap();
        let mut buf = [0u8; 4];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"able");
    }

    #[test]
    fn advise_of_pid_is_noop() {
        let store = SimpleTempDir::new().unwrap();
        store.advise_of_pid(12345).unwrap();
    }

    #[test]
    fn advise_pid_terminated_is_noop() {
        let store = SimpleTempDir::new().unwrap();
        store.advise_pid_terminated(12345).unwrap();
    }

    #[test]
    fn multiple_content_ids_coexist() {
        let store = SimpleTempDir::new().unwrap();
        let id1 = ContentId::for_bytes(b"one");
        let id2 = ContentId::for_bytes(b"two");
        let id3 = ContentId::for_bytes(b"three");
        let lease = LeaseId::new();

        store.store(id1, b"one", lease).unwrap();
        store.store(id2, b"two", lease).unwrap();
        store.store(id3, b"three", lease).unwrap();

        assert_eq!(store.get_data(id1, lease).unwrap(), b"one");
        assert_eq!(store.get_data(id2, lease).unwrap(), b"two");
        assert_eq!(store.get_data(id3, lease).unwrap(), b"three");
    }

    #[test]
    fn advise_lease_dropped_decrements_refcount() {
        let store = SimpleTempDir::new().unwrap();
        let content_id = ContentId::for_bytes(b"ref test");
        let lease1 = LeaseId::new();
        let lease2 = LeaseId::new();

        store.store(content_id, b"ref test", lease1).unwrap();
        // Add another ref via store
        store.store(content_id, b"ref test", lease2).unwrap();

        // Drop one lease — file should still exist (ref count > 0)
        store.advise_lease_dropped(lease1, content_id).unwrap();

        // Data should still be accessible
        assert_eq!(store.get_data(content_id, lease2).unwrap(), b"ref test");
    }
}
