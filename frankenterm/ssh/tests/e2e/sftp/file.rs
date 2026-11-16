use crate::sshd::*;
use assert_fs::prelude::*;
use assert_fs::TempDir;
use frankenterm_ssh::runtime::block_on;
use frankenterm_ssh::runtime::io::{AsyncReadExt, AsyncWriteExt};
use frankenterm_ssh::FilePermissions;
use rstest::*;
use std::convert::TryInto;
use std::path::PathBuf;

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
fn metadata_should_retrieve_file_stat(#[future] session: SessionWithSshd) {
    if !sshd_available() {
        return;
    }
    block_on(async {
        let session: SessionWithSshd = session.await;

        let temp = TempDir::new().unwrap();
        let file = temp.child("test-file");
        file.touch().unwrap();

        let remote_file = session
            .sftp()
            .open(file.path().to_path_buf())
            .await
            .expect("Failed to open remote file");

        let metadata = remote_file
            .metadata()
            .await
            .expect("Failed to read file metadata");

        // Verify that file stat makes sense
        assert!(metadata.is_file(), "Invalid file metadata returned");
    })
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
fn read_dir_should_retrieve_next_dir_entry(#[future] session: SessionWithSshd) {
    if !sshd_available() {
        return;
    }
    block_on(async {
        let session: SessionWithSshd = session.await;

        let temp = TempDir::new().unwrap();
        let dir = temp.child("dir");
        dir.create_dir_all().unwrap();
        let file = temp.child("file");
        file.touch().unwrap();
        let link = temp.child("link");
        link.symlink_to_file(file.path()).unwrap();

        let remote_dir = session
            .sftp()
            .open_dir(temp.path().to_path_buf())
            .await
            .expect("Failed to open remote directory");

        // Collect all of the directory contents (. and .. are included)
        let mut contents = Vec::new();
        while let Ok((path, metadata)) = remote_dir.read_dir().await {
            let ft = metadata.ty;
            contents.push((
                path,
                if ft.is_dir() {
                    "dir"
                } else if ft.is_file() {
                    "file"
                } else {
                    "symlink"
                },
            ));
        }
        contents.sort_unstable_by_key(|(p, _)| p.to_path_buf());

        assert_eq!(
            contents,
            vec![
                (PathBuf::from(".").try_into().unwrap(), "dir"),
                (PathBuf::from("..").try_into().unwrap(), "dir"),
                (PathBuf::from("dir").try_into().unwrap(), "dir"),
                (PathBuf::from("file").try_into().unwrap(), "file"),
                (PathBuf::from("link").try_into().unwrap(), "symlink"),
            ]
        );
    })
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
fn should_support_async_reading(#[future] session: SessionWithSshd) {
    if !sshd_available() {
        return;
    }
    block_on(async {
        let session: SessionWithSshd = session.await;

        let temp = TempDir::new().unwrap();
        let file = temp.child("test-file");
        file.write_str("some file contents").unwrap();

        let mut remote_file = session
            .sftp()
            .open(file.path().to_path_buf())
            .await
            .expect("Failed to open remote file");

        let mut contents = String::new();
        remote_file
            .read_to_string(&mut contents)
            .await
            .expect("Failed to read file to string");

        assert_eq!(contents, "some file contents");

        // NOTE: Testing second time to ensure future is properly cleared
        let mut contents = String::new();
        remote_file
            .read_to_string(&mut contents)
            .await
            .expect("Failed to read file to string second time");
    })
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
fn should_support_async_writing(#[future] session: SessionWithSshd) {
    if !sshd_available() {
        return;
    }
    block_on(async {
        let session: SessionWithSshd = session.await;

        let temp = TempDir::new().unwrap();
        let file = temp.child("test-file");
        file.write_str("some file contents").unwrap();

        let mut remote_file = session
            .sftp()
            .create(file.path().to_path_buf())
            .await
            .expect("Failed to open remote file");

        remote_file
            .write_all(b"new contents for file")
            .await
            .expect("Failed to write to file");

        file.assert("new contents for file");

        // NOTE: Testing second time to ensure future is properly cleared
        remote_file
            .write_all(b"new contents for file")
            .await
            .expect("Failed to write to file second time");
    })
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
fn should_support_async_flush(#[future] session: SessionWithSshd) {
    if !sshd_available() {
        return;
    }
    block_on(async {
        let session: SessionWithSshd = session.await;

        let temp = TempDir::new().unwrap();
        let file = temp.child("test-file");
        file.write_str("some file contents").unwrap();

        let mut remote_file = session
            .sftp()
            .create(file.path().to_path_buf())
            .await
            .expect("Failed to open remote file");

        remote_file.flush().await.expect("Failed to flush file");

        // NOTE: Testing second time to ensure future is properly cleared
        remote_file
            .flush()
            .await
            .expect("Failed to flush file second time");
    })
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
fn should_support_async_close(#[future] session: SessionWithSshd) {
    if !sshd_available() {
        return;
    }
    block_on(async {
        let session: SessionWithSshd = session.await;

        let temp = TempDir::new().unwrap();
        let file = temp.child("test-file");
        file.write_str("some file contents").unwrap();

        let mut remote_file = session
            .sftp()
            .create(file.path().to_path_buf())
            .await
            .expect("Failed to open remote file");

        remote_file.close().await.expect("Failed to close file");

        // NOTE: Testing second time to ensure future is properly cleared
        remote_file
            .close()
            .await
            .expect("Failed to close file second time");
    })
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
fn set_metadata_should_update_permissions_for_an_open_file(#[future] session: SessionWithSshd) {
    if !sshd_available() {
        return;
    }
    block_on(async {
        let session: SessionWithSshd = session.await;

        let temp = TempDir::new().unwrap();
        let file = temp.child("test-file");
        file.write_str("some file contents").unwrap();

        let remote_file = session
            .sftp()
            .open(file.path().to_path_buf())
            .await
            .expect("Failed to open remote file");

        let metadata = remote_file
            .metadata()
            .await
            .expect("Failed to read file metadata");

        let updated = frankenterm_ssh::Metadata {
            permissions: Some(FilePermissions::from_unix_mode(0o600)),
            ..metadata
        };

        remote_file
            .set_metadata(updated)
            .await
            .expect("Failed to update file permissions");

        let actual = remote_file
            .metadata()
            .await
            .expect("Failed to read updated metadata");
        assert_eq!(
            actual.permissions,
            Some(FilePermissions::from_unix_mode(0o600))
        );
        assert!(
            actual.is_file(),
            "Updated metadata should still describe a file"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = std::fs::metadata(file.path())
                .expect("Failed to stat local file after remote chmod")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    })
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
fn set_metadata_should_update_modified_time_for_an_open_file(#[future] session: SessionWithSshd) {
    if !sshd_available() {
        return;
    }
    block_on(async {
        let session: SessionWithSshd = session.await;

        let temp = TempDir::new().unwrap();
        let file = temp.child("test-file");
        file.write_str("some file contents").unwrap();

        let remote_file = session
            .sftp()
            .open(file.path().to_path_buf())
            .await
            .expect("Failed to open remote file");

        let metadata = remote_file
            .metadata()
            .await
            .expect("Failed to read file metadata");
        let target_modified = 1_700_000_123;
        let updated = frankenterm_ssh::Metadata {
            modified: Some(target_modified),
            ..metadata
        };

        remote_file
            .set_metadata(updated)
            .await
            .expect("Failed to update file modification time");

        let actual = remote_file
            .metadata()
            .await
            .expect("Failed to read updated metadata");
        assert_eq!(actual.modified, Some(target_modified));
    })
}

#[rstest]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), ignore)]
fn set_metadata_should_reject_access_time_mutation_for_an_open_file(
    #[future] session: SessionWithSshd,
) {
    if !sshd_available() {
        return;
    }
    block_on(async {
        let session: SessionWithSshd = session.await;

        let temp = TempDir::new().unwrap();
        let file = temp.child("test-file");
        file.write_str("some file contents").unwrap();

        let remote_file = session
            .sftp()
            .open(file.path().to_path_buf())
            .await
            .expect("Failed to open remote file");

        let metadata = remote_file
            .metadata()
            .await
            .expect("Failed to read file metadata");

        let updated = frankenterm_ssh::Metadata {
            accessed: Some(metadata.accessed.unwrap_or_default().saturating_add(60)),
            ..metadata
        };

        let err = remote_file
            .set_metadata(updated)
            .await
            .expect_err("access-time mutation should be rejected for libssh-backed SFTP files");
        let err_text = format!("{err:#}");
        assert!(err_text.contains("access-time changes"), "{}", err_text);
    })
}
