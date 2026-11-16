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

/// Whether `receipt_id` has the canonical shape produced by
/// [`SteeringReceipt::compute_id`]: `steer:` followed by exactly 32 lowercase
/// hex characters. The store refuses any other id BEFORE touching the
/// filesystem, so a caller-supplied id (e.g. `ft steer run --receipt <id>`)
/// can never traverse outside the store directory (`..`, path separators,
/// absolute paths) or alias another receipt via case variance.
#[must_use]
pub fn is_valid_receipt_id(receipt_id: &str) -> bool {
    receipt_id.strip_prefix("steer:").is_some_and(|hex| {
        hex.len() == 32
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// The on-disk path for a receipt id. The id (`steer:<32 hex>`) is mapped to a
/// safe filename by replacing the `:` separator. Fails closed with
/// `InvalidInput` for any id that is not canonically shaped (see
/// [`is_valid_receipt_id`]) — this is the path-traversal guard for
/// caller-supplied ids.
///
/// # Errors
/// `InvalidInput` if `receipt_id` is not `steer:<32 lowercase hex>`.
pub fn receipt_path(ft_dir: &Path, receipt_id: &str) -> std::io::Result<PathBuf> {
    if !is_valid_receipt_id(receipt_id) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "invalid steering receipt id {receipt_id:?} (expected steer:<32 lowercase hex>)"
            ),
        ));
    }
    let safe = receipt_id.replace(':', "_");
    Ok(receipts_dir(ft_dir).join(format!("{safe}.json")))
}

/// Persist a receipt under `ft_dir`. Write-once semantics: because the id is
/// content-addressed, re-persisting an identical receipt simply rewrites
/// identical bytes (idempotent). Returns the path written.
///
/// # Errors
/// `InvalidInput` if the receipt's id is not canonically shaped, I/O errors
/// creating the directory or writing the file, or serialization failure.
pub fn persist_receipt(ft_dir: &Path, receipt: &SteeringReceipt) -> std::io::Result<PathBuf> {
    let path = receipt_path(ft_dir, &receipt.receipt_id)?;
    let dir = receipts_dir(ft_dir);
    std::fs::create_dir_all(&dir)?;
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
/// `InvalidInput` if `receipt_id` is not canonically shaped (fail closed — a
/// malformed id is refused, never resolved to a path), I/O errors other than
/// not-found, or malformed JSON.
pub fn load_receipt(ft_dir: &Path, receipt_id: &str) -> std::io::Result<Option<SteeringReceipt>> {
    let path = receipt_path(ft_dir, receipt_id)?;
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
        let got = load_receipt(dir.path(), "steer:deadbeefdeadbeefdeadbeefdeadbeef").expect("load");
        assert!(got.is_none());
    }

    #[test]
    fn non_canonical_ids_are_refused_before_filesystem_access() {
        // Path-traversal / aliasing guard: a caller-supplied id that is not
        // exactly `steer:<32 lowercase hex>` must be refused, never resolved
        // to a path (so `..`, separators, absolute paths, case variance, and
        // wrong-length ids can never escape or alias the store).
        let dir = tempfile::tempdir().expect("tempdir");
        let bad_ids = [
            "../../../etc/passwd",
            "steer:../../../../tmp/evil",
            "steer:deadbeef",                           // too short
            "steer:deadbeefdeadbeefdeadbeefdeadbeefdd", // too long
            "steer:DEADBEEFDEADBEEFDEADBEEFDEADBEEF",   // uppercase alias
            "steer:deadbeefdeadbeefdeadbeefdeadbeeg",   // non-hex
            "steer:/eadbeefdeadbeefdeadbeefdeadbeef",   // separator
            "plan:deadbeefdeadbeefdeadbeefdeadbeef",    // wrong prefix
            "",
        ];
        for id in bad_ids {
            assert!(!is_valid_receipt_id(id), "id {id:?} must be invalid");
            let err = load_receipt(dir.path(), id).expect_err("must refuse");
            assert_eq!(
                err.kind(),
                std::io::ErrorKind::InvalidInput,
                "id {id:?} must fail closed with InvalidInput"
            );
        }
        // The canonical shape stays valid.
        assert!(is_valid_receipt_id(
            "steer:0123456789abcdef0123456789abcdef"
        ));
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
        let path = receipt_path(dir, "steer:abc123abc123abc123abc123abc123ab").expect("valid id");
        // No `:` in the filename (would be invalid on some filesystems).
        assert!(!path.file_name().unwrap().to_string_lossy().contains(':'));
        assert!(
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("steer_abc123")
        );
        // The resolved path must stay inside the store directory.
        assert!(path.starts_with(receipts_dir(dir)));
    }
}
