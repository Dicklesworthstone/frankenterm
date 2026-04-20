//! Property-based tests for `config::WorkspaceLayout`.
//!
//! Validates constructor path invariants for relative vs absolute storage/ipc
//! configuration and derived workspace directories.

use frankenterm_core::config::{IpcConfig, StorageConfig, WorkspaceLayout};
use proptest::prelude::*;
use std::path::PathBuf;

fn arb_segment() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9_.-]{1,12}".prop_map(String::from)
}

fn arb_root_path() -> impl Strategy<Value = PathBuf> {
    prop::collection::vec(arb_segment(), 1..4).prop_map(|segments| {
        let mut path = PathBuf::from("/tmp");
        for segment in segments {
            path.push(segment);
        }
        path
    })
}

fn arb_relative_path() -> impl Strategy<Value = String> {
    prop::collection::vec(arb_segment(), 1..3).prop_map(|segments| segments.join("/"))
}

fn arb_absolute_path() -> impl Strategy<Value = String> {
    prop::collection::vec(arb_segment(), 1..4).prop_map(|segments| format!("/{}", segments.join("/")))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn workspace_layout_relative_storage_path_stays_under_ft_dir(
        root in arb_root_path(),
        db_path in arb_relative_path(),
    ) {
        let storage = StorageConfig {
            db_path: db_path.clone(),
            ..StorageConfig::default()
        };
        let ipc = IpcConfig::default();

        let layout = WorkspaceLayout::new(root.clone(), &storage, &ipc);

        prop_assert_eq!(&layout.root, &root);
        prop_assert_eq!(&layout.ft_dir, &layout.root.join(".ft"));
        prop_assert_eq!(&layout.db_path, &layout.ft_dir.join(db_path));
        prop_assert!(layout.db_path.starts_with(&layout.ft_dir));
    }

    #[test]
    fn workspace_layout_absolute_paths_are_preserved(
        root in arb_root_path(),
        db_path in arb_absolute_path(),
        socket_path in arb_absolute_path(),
    ) {
        let storage = StorageConfig {
            db_path: db_path.clone(),
            ..StorageConfig::default()
        };
        let ipc = IpcConfig {
            socket_path: socket_path.clone(),
            ..IpcConfig::default()
        };

        let layout = WorkspaceLayout::new(root, &storage, &ipc);

        prop_assert_eq!(&layout.db_path, &PathBuf::from(db_path));
        prop_assert_eq!(&layout.ipc_socket_path, &PathBuf::from(socket_path));
    }

    #[test]
    fn workspace_layout_derived_paths_follow_ft_structure(
        root in arb_root_path(),
        socket_path in arb_relative_path(),
    ) {
        let storage = StorageConfig::default();
        let ipc = IpcConfig {
            socket_path: socket_path.clone(),
            ..IpcConfig::default()
        };

        let layout = WorkspaceLayout::new(root.clone(), &storage, &ipc);

        prop_assert_eq!(&layout.ft_dir, &root.join(".ft"));
        prop_assert_eq!(&layout.logs_dir, &layout.ft_dir.join("logs"));
        prop_assert_eq!(&layout.log_path, &layout.logs_dir.join("ft-watch.log"));
        prop_assert_eq!(&layout.crash_dir, &layout.ft_dir.join("crash"));
        prop_assert_eq!(&layout.diag_dir, &layout.ft_dir.join("diag"));
        prop_assert_eq!(&layout.lock_path, &layout.ft_dir.join("watch.lock"));
        prop_assert_eq!(&layout.ipc_socket_path, &layout.ft_dir.join(socket_path));
        prop_assert_eq!(&layout.recordings_dir(), &layout.ft_dir.join("recordings"));
    }
}
