//! ft-7h5da.6.3 prerequisite: a file-backed steering-receipt store.
//!
//! Steering receipts are immutable, content-addressed artifacts (the
//! `receipt_id` *is* a hash of the binding), so a file-per-receipt under
//! `<workspace>/.ft/steer_receipts/<id>.json` is the natural store — write-once,
//! look-up-by-id, no DB schema migration. This removes the "receipts are only
//! printed, never persisted" blocker so the execute half of W5.3
//! (`ft steer run --receipt <id>`) has a receipt source to load + revalidate.

use crate::steering::SteeringReceipt;
use std::path::{Path, PathBuf};

/// The directory holding persisted receipts under a workspace's `.ft` dir.
#[must_use]
pub fn receipts_dir(ft_dir: &Path) -> PathBuf {
    ft_dir.join("steer_receipts")
}

/// The on-disk path for a receipt id. The id (`steer:<hex>`) is mapped to a safe
/// filename by replacing the `:` separator.
#[must_use]
pub fn receipt_path(ft_dir: &Path, receipt_id: &str) -> PathBuf {
    let safe = receipt_id.replace(':', "_");
    receipts_dir(ft_dir).join(format!("{safe}.json"))
}

/// Persist a receipt under `ft_dir`. Write-once semantics: because the id is
/// content-addressed, re-persisting an identical receipt simply rewrites
/// identical bytes (idempotent). Returns the path written.
///
/// # Errors
/// I/O errors creating the directory or writing the file, or serialization
/// failure.
pub fn persist_receipt(ft_dir: &Path, receipt: &SteeringReceipt) -> std::io::Result<PathBuf> {
    let dir = receipts_dir(ft_dir);
    std::fs::create_dir_all(&dir)?;
    let path = receipt_path(ft_dir, &receipt.receipt_id);
    let json = serde_json::to_string_pretty(receipt)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&path, json)?;
    Ok(path)
}

/// Load a receipt by id. Returns `Ok(None)` if no such receipt is stored.
///
/// The returned receipt is NOT yet trusted: the caller must
/// [`SteeringReceipt::validate`] it (id-binding integrity) and run it through
/// the revalidation gate before acting on it.
///
/// # Errors
/// I/O errors other than not-found, or malformed JSON.
pub fn load_receipt(ft_dir: &Path, receipt_id: &str) -> std::io::Result<Option<SteeringReceipt>> {
    let path = receipt_path(ft_dir, receipt_id);
    match std::fs::read_to_string(&path) {
        Ok(s) => {
            let receipt: SteeringReceipt = serde_json::from_str(&s)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            Ok(Some(receipt))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(objective: &str) -> SteeringReceipt {
        SteeringReceipt::new(
            objective,
            "ws",
            None,
            Some("plan-hash".to_string()),
            "envelope.admit",
            Some(900),
            Vec::new(),
            1_700_000_000_000,
            Some(10_000),
        )
    }

    #[test]
    fn persist_then_load_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let receipt = sample("ship it");
        let path = persist_receipt(dir.path(), &receipt).expect("persist");
        assert!(path.exists(), "receipt file must exist");
        assert!(path.starts_with(receipts_dir(dir.path())));

        let loaded = load_receipt(dir.path(), &receipt.receipt_id)
            .expect("load")
            .expect("present");
        assert_eq!(loaded, receipt);
        assert!(loaded.validate().is_ok());
    }

    #[test]
    fn load_absent_receipt_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let got = load_receipt(dir.path(), "steer:deadbeef").expect("load");
        assert!(got.is_none());
    }

    #[test]
    fn persist_is_idempotent_for_same_receipt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let receipt = sample("same");
        let p1 = persist_receipt(dir.path(), &receipt).expect("persist 1");
        let p2 = persist_receipt(dir.path(), &receipt).expect("persist 2");
        assert_eq!(p1, p2, "same content-addressed id -> same path");
        let loaded = load_receipt(dir.path(), &receipt.receipt_id)
            .expect("load")
            .expect("present");
        assert_eq!(loaded.receipt_id, receipt.receipt_id);
    }

    #[test]
    fn receipt_id_separator_is_filename_safe() {
        let dir = std::path::Path::new("/tmp/ft");
        let path = receipt_path(dir, "steer:abc123");
        // No `:` in the filename (would be invalid on some filesystems).
        assert!(!path.file_name().unwrap().to_string_lossy().contains(':'));
        assert!(
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("steer_abc123")
        );
    }
}
