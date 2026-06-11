//! ft-7h5da.3.5: durable idempotency store for verified-submit sends.
//!
//! Maps an idempotency key (see [`crate::verified_submit::idempotency_key`]) to
//! the [`VerifiedSubmitReport`] of the send that claimed it, so a meta-agent
//! replaying an identical `(pane, text, key)` after a delivered/queued send
//! recovers the ORIGINAL receipt instead of double-submitting the prompt.
//!
//! File-per-key under `<workspace>/.ft/submit_idempotency/<key>.json` — the key
//! is already a content hash, so no DB schema migration is needed.
//!
//! Divergence from the suggested `profiles_applied_log` append-replay pattern: a
//! file-per-key overwrite store has the same latest-per-key lookup semantics
//! with less machinery and no storage migration. The decision over the looked-up
//! state still lives in [`crate::verified_submit::idempotency_outcome`].

use crate::verified_submit::{IdempotencyOutcome, VerifiedSubmitReport, idempotency_outcome};
use std::path::{Path, PathBuf};

fn store_dir(ft_dir: &Path) -> PathBuf {
    ft_dir.join("submit_idempotency")
}

/// Whether `key` has the canonical shape produced by
/// [`crate::verified_submit::idempotency_key`]: `idem:<decimal pane id>:<16
/// lowercase hex>`. The store refuses any other key BEFORE touching the
/// filesystem, so a caller-supplied key (e.g. a replayed `--idempotency-key`
/// argument) can never traverse outside the store directory (`..`, path
/// separators, absolute paths) or alias another entry via case variance.
#[must_use]
pub fn is_valid_submit_key(key: &str) -> bool {
    let Some(rest) = key.strip_prefix("idem:") else {
        return false;
    };
    let Some((pane, digest)) = rest.split_once(':') else {
        return false;
    };
    // u64 decimal pane id: digits only, no leading-zero aliasing beyond "0",
    // bounded to u64's 20-digit maximum.
    let pane_ok = !pane.is_empty()
        && pane.len() <= 20
        && pane.bytes().all(|b| b.is_ascii_digit())
        && (pane == "0" || !pane.starts_with('0'));
    let digest_ok = digest.len() == 16
        && digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    pane_ok && digest_ok
}

/// The on-disk path for an idempotency key (`:` mapped to a filename-safe `_`).
/// Fails closed with `InvalidInput` for any key that is not canonically shaped
/// (see [`is_valid_submit_key`]) — this is the path-traversal guard for
/// caller-supplied keys.
///
/// # Errors
/// `InvalidInput` if `key` is not `idem:<decimal pane>:<16 lowercase hex>`.
pub fn key_path(ft_dir: &Path, key: &str) -> std::io::Result<PathBuf> {
    if !is_valid_submit_key(key) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid submit idempotency key {key:?} (expected idem:<pane>:<16 hex>)"),
        ));
    }
    let safe = key.replace(':', "_");
    Ok(store_dir(ft_dir).join(format!("{safe}.json")))
}

/// Record the receipt that claimed an idempotency key. Overwrites with the
/// latest report for the key (e.g. a `queued_behind_operation` that later
/// resolves to `submitted`). Returns the path written.
///
/// # Errors
/// `InvalidInput` if `key` is not canonically shaped, I/O errors creating the
/// directory or writing the file, or serialization failure.
pub fn record(
    ft_dir: &Path,
    key: &str,
    report: &VerifiedSubmitReport,
) -> std::io::Result<PathBuf> {
    let path = key_path(ft_dir, key)?;
    let dir = store_dir(ft_dir);
    std::fs::create_dir_all(&dir)?;
    let json = serde_json::to_string_pretty(report)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&path, json)?;
    Ok(path)
}

/// Look up the prior receipt for an idempotency key. `Ok(None)` if never claimed.
///
/// # Errors
/// `InvalidInput` if `key` is not canonically shaped (fail closed — a
/// malformed key is refused, never resolved to a path), I/O errors other than
/// not-found, or malformed JSON.
pub fn lookup(ft_dir: &Path, key: &str) -> std::io::Result<Option<VerifiedSubmitReport>> {
    let path = key_path(ft_dir, key)?;
    match std::fs::read_to_string(&path) {
        Ok(s) => Ok(Some(
            serde_json::from_str(&s)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// The single entry point a send path calls before injecting a prompt: look up
/// the key and decide whether the send is a duplicate, returning the prior
/// report to hand back on a [`IdempotencyOutcome::DuplicateNoop`].
///
/// # Errors
/// I/O / deserialization errors from [`lookup`].
pub fn decide(
    ft_dir: &Path,
    key: &str,
) -> std::io::Result<(IdempotencyOutcome, Option<VerifiedSubmitReport>)> {
    let prior = lookup(ft_dir, key)?;
    let outcome = idempotency_outcome(prior.as_ref().map(|r| r.state));
    Ok((outcome, prior))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::robot_types::SubmitReceiptState;

    fn report(state: SubmitReceiptState) -> VerifiedSubmitReport {
        VerifiedSubmitReport {
            state,
            agent_type: Some("codex".to_string()),
            profile_id: Some("codex.default".to_string()),
            profile_version: None,
            attempts: 1,
            evidence_rule_ids: vec!["submit_profile:codex.default:submitted:0".to_string()],
            polls: 1,
            cursor_before: None,
            cursor_after: None,
        }
    }

    /// A canonically-shaped key (as produced by `idempotency_key`).
    fn key(pane: u64, hex16: &str) -> String {
        assert_eq!(hex16.len(), 16, "test key digest must be 16 hex chars");
        format!("idem:{pane}:{hex16}")
    }

    #[test]
    fn record_then_lookup_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let r = report(SubmitReceiptState::Submitted);
        let k = key(7, "0123456789abcdef");
        let path = record(dir.path(), &k, &r).expect("record");
        assert!(path.exists());
        let got = lookup(dir.path(), &k).expect("lookup").expect("present");
        assert_eq!(got, r);
    }

    #[test]
    fn lookup_absent_key_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let k = key(7, "feedfacefeedface");
        assert!(lookup(dir.path(), &k).expect("lookup").is_none());
    }

    #[test]
    fn decide_no_prior_proceeds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let k = key(1, "00000000000000aa");
        let (outcome, prior) = decide(dir.path(), &k).expect("decide");
        assert_eq!(outcome, IdempotencyOutcome::Proceed);
        assert!(prior.is_none());
    }

    #[test]
    fn decide_after_submitted_is_duplicate_with_original_receipt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let k = key(1, "d0d0d0d0d0d0d0d0");
        record(dir.path(), &k, &report(SubmitReceiptState::Submitted)).expect("record");
        let (outcome, prior) = decide(dir.path(), &k).expect("decide");
        assert_eq!(outcome, IdempotencyOutcome::DuplicateNoop);
        assert_eq!(prior.expect("prior").state, SubmitReceiptState::Submitted);
    }

    #[test]
    fn decide_after_failed_send_proceeds_for_retry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let k = key(1, "fa11edfa11edfa11");
        record(dir.path(), &k, &report(SubmitReceiptState::SendFailed)).expect("record");
        let (outcome, _) = decide(dir.path(), &k).expect("decide");
        assert_eq!(outcome, IdempotencyOutcome::Proceed);
    }

    #[test]
    fn key_separator_is_filename_safe() {
        let path = key_path(Path::new("/tmp/ft"), "idem:7:0123456789abcdef").expect("valid key");
        assert!(!path.file_name().unwrap().to_string_lossy().contains(':'));
        assert!(path.starts_with(store_dir(Path::new("/tmp/ft"))));
    }

    #[test]
    fn non_canonical_keys_are_refused_before_filesystem_access() {
        // Path-traversal / aliasing guard: a caller-supplied key that is not
        // exactly `idem:<decimal pane>:<16 lowercase hex>` must be refused,
        // never resolved to a path.
        let dir = tempfile::tempdir().expect("tempdir");
        let bad_keys = [
            "../../../etc/passwd",
            "idem:7:../../../tmp/evil",
            "idem:7:abc",                  // digest too short
            "idem:7:0123456789abcdef0",    // digest too long
            "idem:7:0123456789ABCDEF",     // uppercase alias
            "idem:7:0123456789abcdeg",     // non-hex
            "idem:7:/123456789abcdef",     // separator
            "idem::0123456789abcdef",      // empty pane
            "idem:x7:0123456789abcdef",    // non-decimal pane
            "idem:07:0123456789abcdef",    // leading-zero pane alias
            "steer:7:0123456789abcdef",    // wrong prefix
            "",
        ];
        for k in bad_keys {
            assert!(!is_valid_submit_key(k), "key {k:?} must be invalid");
            let err = lookup(dir.path(), k).expect_err("must refuse");
            assert_eq!(
                err.kind(),
                std::io::ErrorKind::InvalidInput,
                "key {k:?} must fail closed with InvalidInput"
            );
        }
        // The canonical shape stays valid, including pane 0 and u64::MAX.
        assert!(is_valid_submit_key("idem:0:0123456789abcdef"));
        assert!(is_valid_submit_key("idem:18446744073709551615:0123456789abcdef"));
        // Round-trip against the real generator.
        let generated = crate::verified_submit::idempotency_key(42, "deploy now", Some("nonce"));
        assert!(
            is_valid_submit_key(&generated),
            "generator output {generated:?} must pass the guard"
        );
    }
}
