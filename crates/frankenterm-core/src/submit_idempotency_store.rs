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

/// The on-disk path for an idempotency key (`:` mapped to a filename-safe `_`).
#[must_use]
pub fn key_path(ft_dir: &Path, key: &str) -> PathBuf {
    let safe = key.replace(':', "_");
    store_dir(ft_dir).join(format!("{safe}.json"))
}

/// Record the receipt that claimed an idempotency key. Overwrites with the
/// latest report for the key (e.g. a `queued_behind_operation` that later
/// resolves to `submitted`). Returns the path written.
///
/// # Errors
/// I/O errors creating the directory or writing the file, or serialization
/// failure.
pub fn record(
    ft_dir: &Path,
    key: &str,
    report: &VerifiedSubmitReport,
) -> std::io::Result<PathBuf> {
    let dir = store_dir(ft_dir);
    std::fs::create_dir_all(&dir)?;
    let path = key_path(ft_dir, key);
    let json = serde_json::to_string_pretty(report)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&path, json)?;
    Ok(path)
}

/// Look up the prior receipt for an idempotency key. `Ok(None)` if never claimed.
///
/// # Errors
/// I/O errors other than not-found, or malformed JSON.
pub fn lookup(ft_dir: &Path, key: &str) -> std::io::Result<Option<VerifiedSubmitReport>> {
    let path = key_path(ft_dir, key);
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

    #[test]
    fn record_then_lookup_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let r = report(SubmitReceiptState::Submitted);
        let path = record(dir.path(), "idem:7:abc", &r).expect("record");
        assert!(path.exists());
        let got = lookup(dir.path(), "idem:7:abc")
            .expect("lookup")
            .expect("present");
        assert_eq!(got, r);
    }

    #[test]
    fn lookup_absent_key_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(lookup(dir.path(), "idem:7:missing").expect("lookup").is_none());
    }

    #[test]
    fn decide_no_prior_proceeds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (outcome, prior) = decide(dir.path(), "idem:1:none").expect("decide");
        assert_eq!(outcome, IdempotencyOutcome::Proceed);
        assert!(prior.is_none());
    }

    #[test]
    fn decide_after_submitted_is_duplicate_with_original_receipt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = "idem:1:done";
        record(dir.path(), key, &report(SubmitReceiptState::Submitted)).expect("record");
        let (outcome, prior) = decide(dir.path(), key).expect("decide");
        assert_eq!(outcome, IdempotencyOutcome::DuplicateNoop);
        assert_eq!(prior.expect("prior").state, SubmitReceiptState::Submitted);
    }

    #[test]
    fn decide_after_failed_send_proceeds_for_retry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = "idem:1:failed";
        record(dir.path(), key, &report(SubmitReceiptState::SendFailed)).expect("record");
        let (outcome, _) = decide(dir.path(), key).expect("decide");
        assert_eq!(outcome, IdempotencyOutcome::Proceed);
    }

    #[test]
    fn key_separator_is_filename_safe() {
        let path = key_path(Path::new("/tmp/ft"), "idem:7:abc123");
        assert!(!path.file_name().unwrap().to_string_lossy().contains(':'));
    }
}
