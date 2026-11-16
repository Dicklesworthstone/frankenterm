//! Property tests for storage configuration mmap scrollback sizing.

use frankenterm_core::config::{Config, StorageConfig};
use proptest::prelude::*;

fn storage_config_with_cap(cap_mb: u32) -> StorageConfig {
    StorageConfig {
        scrollback_mmap_cap_mb: cap_mb,
        ..StorageConfig::default()
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn explicit_mmap_cap_toml_value_is_preserved(cap_mb in any::<u32>()) {
        let raw = format!("scrollback_mmap_cap_mb = {cap_mb}\n");
        let config: StorageConfig = toml::from_str(&raw).unwrap();

        prop_assert_eq!(config.scrollback_mmap_cap_mb, cap_mb);
        prop_assert!(config.validate().is_ok());
    }

    #[test]
    fn storage_config_json_roundtrip_preserves_mmap_cap(cap_mb in any::<u32>()) {
        let config = storage_config_with_cap(cap_mb);

        let json = serde_json::to_string(&config).unwrap();
        let decoded: StorageConfig = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(decoded.scrollback_mmap_cap_mb, cap_mb);
        prop_assert_eq!(decoded.db_path, config.db_path);
        prop_assert_eq!(decoded.retention_days, config.retention_days);
        prop_assert_eq!(decoded.writer_queue_size, config.writer_queue_size);
    }

    #[test]
    fn full_config_storage_table_preserves_mmap_cap(cap_mb in any::<u32>()) {
        let raw = format!("[storage]\nscrollback_mmap_cap_mb = {cap_mb}\n");
        let config: Config = toml::from_str(&raw).unwrap();

        prop_assert_eq!(config.storage.scrollback_mmap_cap_mb, cap_mb);
        prop_assert!(config.validate().is_ok());
    }

    #[test]
    fn missing_mmap_cap_uses_default_with_arbitrary_retention(retention_days in any::<u32>()) {
        let raw = format!("retention_days = {retention_days}\n");
        let config: StorageConfig = toml::from_str(&raw).unwrap();

        prop_assert_eq!(config.retention_days, retention_days);
        prop_assert_eq!(
            config.scrollback_mmap_cap_mb,
            StorageConfig::default().scrollback_mmap_cap_mb
        );
    }

    #[test]
    fn mmap_cap_does_not_change_writer_queue_validation(
        cap_mb in any::<u32>(),
        writer_queue_size in 0u32..=4,
    ) {
        let config = StorageConfig {
            scrollback_mmap_cap_mb: cap_mb,
            writer_queue_size,
            ..StorageConfig::default()
        };

        prop_assert_eq!(config.validate().is_ok(), writer_queue_size > 0);
    }
}
