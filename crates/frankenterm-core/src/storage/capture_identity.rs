//! Immutable capture provenance, separate from live-pane authorization.
//!
//! A capture UUID identifies recorded output. Native identity records which
//! mux registration supplied that capture; it does not enroll a host, grant
//! remote access, or authorize a live attachment. Historical rows without a
//! binding remain readable and must never be backfilled from a recycled pane
//! number. Segment associations come from the admitted producer, not `panes`.

use super::{
    CellRowReader, PaneRecord, Result, Segment, SqlCell, StorageBackend, StorageError, ToSqlValue,
    WriterTransactionBeginMode, execute_typed, now_ms, query_pane_backend, record_gap_backend,
    run_writer_transaction, storage_backend_error, u64_to_i64, upsert_pane_backend,
};
use crate::wezterm::MuxPaneIdentity;

pub(crate) const CAPTURE_IDENTITY_SCHEMA_SQL: &str = r"
CREATE TABLE IF NOT EXISTS pane_capture_bindings (
    capture_uuid TEXT PRIMARY KEY NOT NULL
        CHECK(typeof(capture_uuid) = 'text' AND length(capture_uuid) = 32
            AND capture_uuid NOT GLOB '*[^0-9a-f]*'),
    pane_id INTEGER NOT NULL CHECK(typeof(pane_id) = 'integer' AND pane_id >= 0),
    native_identity_json TEXT
        CHECK(native_identity_json IS NULL OR
            (typeof(native_identity_json) = 'text'
                AND length(CAST(native_identity_json AS BLOB)) <= 512)),
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS output_segment_captures (
    segment_id INTEGER PRIMARY KEY NOT NULL
        REFERENCES output_segments(id) ON DELETE CASCADE,
    capture_uuid TEXT NOT NULL REFERENCES pane_capture_bindings(capture_uuid)
);
CREATE INDEX IF NOT EXISTS idx_output_segment_capture
    ON output_segment_captures(capture_uuid, segment_id);
";

/// Exact provenance of one independent capture lifetime.
///
/// `None` is an explicitly unidentified backend. Even `Some` is local mux
/// provenance only: there is no enrolled host identity or browser capability
/// here. An effect must still query and compare current native identity.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaneCaptureIdentity {
    pub capture_uuid: String,
    pub pane_id: u64,
    pub native_identity: Option<MuxPaneIdentity>,
}

impl PaneCaptureIdentity {
    pub(crate) fn validate(&self) -> Result<()> {
        validate_capture_uuid(&self.capture_uuid)?;
        u64_to_i64(self.pane_id, "capture pane_id")?;
        if self
            .native_identity
            .is_some_and(|identity| !identity.is_valid())
        {
            return Err(StorageError::Database("invalid native capture identity".into()).into());
        }
        Ok(())
    }

    /// A numeric ID or two missing identities never prove continuity.
    #[must_use]
    pub fn matches_native(&self, pane_id: u64, live: Option<&MuxPaneIdentity>) -> bool {
        self.pane_id == pane_id
            && live.is_some_and(|live| {
                live.is_valid() && self.native_identity.as_ref() == Some(live)
            })
    }
}

pub(crate) fn validate_capture_uuid(capture_uuid: &str) -> Result<()> {
    if capture_uuid.len() != 32
        || !capture_uuid
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(StorageError::Database("invalid capture UUID".into()).into());
    }
    Ok(())
}

fn from_row(row: &[SqlCell]) -> Result<PaneCaptureIdentity> {
    let reader = CellRowReader::new(row);
    let pane_id = reader
        .i64(1)
        .map_err(|error| storage_backend_error("Capture identity pane", error))?;
    let native_json = reader
        .optional_string(2)
        .map_err(|error| storage_backend_error("Capture native identity", error))?;
    let native_identity = native_json
        .as_deref()
        .map(|json| {
            serde_json::from_str::<MuxPaneIdentity>(json).map_err(|_| {
                StorageError::Database("invalid stored native capture identity".into())
            })
        })
        .transpose()?;
    let identity = PaneCaptureIdentity {
        capture_uuid: reader
            .string(0)
            .map_err(|error| storage_backend_error("Capture UUID", error))?,
        pane_id: u64::try_from(pane_id)
            .map_err(|_| StorageError::Database("invalid stored capture pane".into()))?,
        native_identity,
    };
    identity.validate()?;
    Ok(identity)
}

pub(crate) fn query_capture_identity_backend(
    backend: &dyn StorageBackend,
    capture_uuid: &str,
) -> Result<Option<PaneCaptureIdentity>> {
    let row = backend
        .query_row_cells(
            "SELECT capture_uuid, pane_id, native_identity_json
             FROM pane_capture_bindings WHERE capture_uuid = ?1",
            &[ToSqlValue::Text(capture_uuid)],
        )
        .map_err(|error| storage_backend_error("Read capture identity", error))?;
    row.as_deref().map(from_row).transpose()
}

pub(crate) fn query_segment_capture_identity_backend(
    backend: &dyn StorageBackend,
    segment_id: i64,
) -> Result<Option<PaneCaptureIdentity>> {
    let row = backend
        .query_row_cells(
            "SELECT binding.capture_uuid, binding.pane_id, binding.native_identity_json,
                    segment.pane_id
             FROM output_segment_captures AS captured
             JOIN pane_capture_bindings AS binding ON binding.capture_uuid = captured.capture_uuid
             JOIN output_segments AS segment ON segment.id = captured.segment_id
             WHERE captured.segment_id = ?1",
            &[ToSqlValue::Integer(segment_id)],
        )
        .map_err(|error| storage_backend_error("Read segment capture identity", error))?;
    row.as_deref()
        .map(|row| {
            let identity = from_row(row)?;
            let segment_pane = CellRowReader::new(row)
                .i64(3)
                .map_err(|error| storage_backend_error("Segment capture pane", error))?;
            if u64::try_from(segment_pane).ok() != Some(identity.pane_id) {
                return Err(
                    StorageError::Database("segment capture identity mismatch".into()).into(),
                );
            }
            Ok(identity)
        })
        .transpose()
}

fn insert_binding_backend(
    backend: &dyn StorageBackend,
    identity: &PaneCaptureIdentity,
) -> Result<()> {
    identity.validate()?;
    let native_json = identity
        .native_identity
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|_| StorageError::Database("cannot encode native capture identity".into()))?;
    execute_typed(
        backend,
        "INSERT INTO pane_capture_bindings
             (capture_uuid, pane_id, native_identity_json, created_at)
         VALUES (?1, ?2, ?3, ?4) ON CONFLICT(capture_uuid) DO NOTHING",
        &[
            ToSqlValue::Text(&identity.capture_uuid),
            ToSqlValue::Integer(u64_to_i64(identity.pane_id, "capture pane_id")?),
            ToSqlValue::optional_text(native_json.as_deref()),
            ToSqlValue::Integer(now_ms()),
        ],
    )
    .map_err(|error| storage_backend_error("Persist capture identity", error))?;
    // Never promote an unidentified capture or rebind an existing UUID to a
    // replacement. This comparison runs under the same writer transaction.
    if query_capture_identity_backend(backend, &identity.capture_uuid)?.as_ref() != Some(identity) {
        return Err(StorageError::Database(
            "capture UUID already has different provenance".into(),
        )
        .into());
    }
    Ok(())
}

/// A restart must supply a fresh proposed UUID; only an exact native match
/// can recover a previous UUID. With resume_previous=false, the caller owns
/// the proposed in-process lifetime and must already have checked continuity.
/// That mode also allows an idempotent retry of the same prepared lifetime.
pub(crate) fn prepare_pane_capture_backend(
    backend: &dyn StorageBackend,
    mut pane: PaneRecord,
    native_identity: Option<MuxPaneIdentity>,
    resume_previous: bool,
) -> Result<PaneCaptureIdentity> {
    let candidate_uuid = pane
        .pane_uuid
        .as_deref()
        .ok_or_else(|| StorageError::Database("capture setup requires a UUID".into()))?;
    validate_capture_uuid(candidate_uuid)?;
    if native_identity.is_some_and(|identity| !identity.is_valid()) {
        return Err(StorageError::Database("invalid native capture identity".into()).into());
    }
    run_writer_transaction(
        backend,
        WriterTransactionBeginMode::Immediate,
        "prepare pane capture identity",
        || {
            let previous = query_pane_backend(backend, pane.pane_id)?;
            let previous_uuid = previous.as_ref().and_then(|row| row.pane_uuid.as_deref());
            let mut recovered_native = false;
            if resume_previous
                && let Some(previous_uuid) = previous_uuid
                && let Some(previous_identity) =
                    query_capture_identity_backend(backend, previous_uuid)?
                && previous_identity.matches_native(pane.pane_id, native_identity.as_ref())
            {
                pane.pane_uuid = Some(previous_identity.capture_uuid);
                recovered_native = true;
            }
            if resume_previous && !recovered_native {
                let candidate_uuid = pane
                    .pane_uuid
                    .as_deref()
                    .ok_or_else(|| StorageError::Database("capture setup lost its UUID".into()))?;
                if previous_uuid == Some(candidate_uuid)
                    || query_capture_identity_backend(backend, candidate_uuid)?.is_some()
                {
                    return Err(StorageError::Database(
                        "capture restart requires a fresh UUID or exact native identity".into(),
                    )
                    .into());
                }
            }
            let identity = PaneCaptureIdentity {
                capture_uuid: pane
                    .pane_uuid
                    .clone()
                    .ok_or_else(|| StorageError::Database("capture setup lost its UUID".into()))?,
                pane_id: pane.pane_id,
                native_identity,
            };
            insert_binding_backend(backend, &identity)?;
            upsert_pane_backend(backend, &pane)?;
            if previous.is_some() && previous_uuid != Some(identity.capture_uuid.as_str()) {
                record_gap_backend(backend, pane.pane_id, "capture_identity_changed", None)?;
            }
            Ok(identity)
        },
    )
}

/// Called inside the segment's owning append transaction, before its reply.
pub(crate) fn record_segment_capture_backend(
    backend: &dyn StorageBackend,
    segment: &Segment,
    capture_uuid: Option<&str>,
) -> Result<()> {
    let Some(capture_uuid) = capture_uuid else {
        return Ok(());
    };
    validate_capture_uuid(capture_uuid)?;
    // One indexed statement binds the admitted producer and rejects a UUID
    // prepared for another numeric pane, without consulting mutable metadata.
    let inserted = backend
        .query_row_cells(
            "INSERT INTO output_segment_captures (segment_id, capture_uuid)
             SELECT ?1, capture_uuid FROM pane_capture_bindings
             WHERE capture_uuid = ?2 AND pane_id = ?3
             RETURNING segment_id",
            &[
                ToSqlValue::Integer(segment.id),
                ToSqlValue::Text(capture_uuid),
                ToSqlValue::Integer(u64_to_i64(segment.pane_id, "capture pane_id")?),
            ],
        )
        .map_err(|error| storage_backend_error("Persist segment capture identity", error))?;
    if inserted.is_none() {
        return Err(StorageError::Database(
            "segment capture identity was not prepared for its producer".into(),
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::{
        PendingAppendSegmentWrite, RusqliteBackend, WriterResultResponder,
        append_segment_commit_with_capture_backend, append_segment_group_commit_backend,
        initialize_schema, pending_append_segment_from_command,
    };
    use crate::runtime_async::oneshot;
    use std::collections::HashMap;

    const CAPTURE_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const CAPTURE_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const CAPTURE_C: &str = "cccccccccccccccccccccccccccccccc";

    fn backend() -> RusqliteBackend {
        let conn = rusqlite::Connection::open_in_memory().expect("open capture identity fixture");
        initialize_schema(&conn).expect("initialize capture identity fixture");
        RusqliteBackend::new(conn).expect("install writer authorizer")
    }

    fn pane(pane_id: u64, capture_uuid: &str) -> PaneRecord {
        PaneRecord {
            pane_id,
            pane_uuid: Some(capture_uuid.to_string()),
            domain: "local".to_string(),
            window_id: Some(1),
            tab_id: Some(1),
            title: None,
            cwd: None,
            tty_name: None,
            first_seen_at: 1,
            last_seen_at: 2,
            observed: true,
            ignore_reason: None,
            last_decision_at: Some(2),
        }
    }

    fn native() -> MuxPaneIdentity {
        MuxPaneIdentity {
            session_incarnation: [1; 16],
            durable_pane_id: [2; 16],
            pane_registration: [3; 16],
        }
    }

    fn append(
        backend: &dyn StorageBackend,
        pane_id: u64,
        capture_uuid: Option<&str>,
    ) -> Result<Segment> {
        append_segment_commit_with_capture_backend(
            backend,
            pane_id,
            "captured output\n",
            None,
            None,
            None,
            &mut HashMap::new(),
            capture_uuid,
        )
        .map(|committed| committed.segment)
    }

    fn pending(pane_id: u64, capture_uuid: &str) -> PendingAppendSegmentWrite {
        let (send, _receive) = oneshot::channel();
        PendingAppendSegmentWrite {
            pane_id,
            content: "queued output\n".to_string(),
            content_hash: None,
            zone_type: None,
            capture_uuid: Some(capture_uuid.to_string()),
            recorder_delivery: None,
            capture_hold: None,
            respond: WriterResultResponder::new(send),
        }
    }

    fn segment_count(backend: &dyn StorageBackend) -> i64 {
        let row = backend
            .query_row_cells("SELECT count(*) FROM output_segments", &[])
            .expect("count segments")
            .expect("count row");
        CellRowReader::new(&row).i64(0).expect("segment count")
    }

    #[test]
    fn exact_native_identity_resumes_the_independent_capture_uuid() {
        let backend = backend();
        let first = prepare_pane_capture_backend(&backend, pane(7, CAPTURE_A), Some(native()), true)
            .expect("prepare first capture");
        let resumed =
            prepare_pane_capture_backend(&backend, pane(7, CAPTURE_B), Some(native()), true)
                .expect("resume authenticated registration");
        assert_eq!(resumed, first);
        assert_eq!(resumed.capture_uuid, CAPTURE_A);
        assert_eq!(resumed.native_identity, Some(native()));
        assert!(query_capture_identity_backend(&backend, CAPTURE_B).unwrap().is_none());
        assert!(!resumed.matches_native(8, Some(&native())));
        assert!(!resumed.matches_native(7, None));
    }

    #[test]
    fn any_native_lifetime_change_refuses_numeric_pane_id_reuse() {
        for replacement in [
            MuxPaneIdentity {
                session_incarnation: [4; 16],
                ..native()
            },
            MuxPaneIdentity {
                durable_pane_id: [4; 16],
                ..native()
            },
            MuxPaneIdentity {
                pane_registration: [4; 16],
                ..native()
            },
        ] {
            let backend = backend();
            prepare_pane_capture_backend(&backend, pane(7, CAPTURE_A), Some(native()), true)
                .unwrap();
            append(&backend, 7, Some(CAPTURE_A)).unwrap();
            let replacement =
                prepare_pane_capture_backend(&backend, pane(7, CAPTURE_B), Some(replacement), true)
                    .expect("start replacement capture");
            assert_eq!(replacement.capture_uuid, CAPTURE_B);
            let row = backend
                .query_row_cells(
                    "SELECT reason FROM output_gaps WHERE pane_id = 7",
                    &[],
                )
                .unwrap()
                .expect("capture lifetime discontinuity");
            assert_eq!(
                CellRowReader::new(&row).string(0).unwrap(),
                "capture_identity_changed"
            );
            assert_eq!(
                query_capture_identity_backend(&backend, CAPTURE_A)
                    .unwrap()
                    .unwrap()
                    .native_identity,
                Some(native())
            );
        }
    }

    #[test]
    fn legacy_and_unidentified_captures_never_gain_invented_continuity() {
        let backend = backend();
        upsert_pane_backend(&backend, &pane(7, CAPTURE_A)).unwrap();
        let historical = append(&backend, 7, None).unwrap();
        let first = prepare_pane_capture_backend(&backend, pane(7, CAPTURE_B), None, true)
            .expect("begin unidentified capture");
        assert_eq!(first.capture_uuid, CAPTURE_B);
        assert_eq!(first.native_identity, None);
        assert!(!first.matches_native(7, None));
        assert!(query_segment_capture_identity_backend(&backend, historical.id).unwrap().is_none());
        assert!(query_capture_identity_backend(&backend, CAPTURE_A).unwrap().is_none());
        let restarted =
            prepare_pane_capture_backend(&backend, pane(7, CAPTURE_C), None, true).unwrap();
        assert_eq!(restarted.capture_uuid, CAPTURE_C);
    }

    #[test]
    fn restart_cannot_reuse_an_unidentified_candidate_uuid() {
        let backend = backend();
        prepare_pane_capture_backend(&backend, pane(7, CAPTURE_A), None, true).unwrap();
        assert!(
            prepare_pane_capture_backend(&backend, pane(7, CAPTURE_A), None, true).is_err()
        );
        // A caller that still owns the same in-process lifetime may retry.
        prepare_pane_capture_backend(&backend, pane(7, CAPTURE_A), None, false).unwrap();
        prepare_pane_capture_backend(&backend, pane(7, CAPTURE_B), None, true).unwrap();
        assert!(
            prepare_pane_capture_backend(&backend, pane(7, CAPTURE_A), None, true).is_err()
        );
    }

    #[test]
    fn existing_capture_uuid_cannot_be_rebound_or_promoted() {
        let backend = backend();
        let original =
            prepare_pane_capture_backend(&backend, pane(7, CAPTURE_A), None, false).unwrap();
        assert!(
            prepare_pane_capture_backend(&backend, pane(7, CAPTURE_A), Some(native()), false)
                .is_err()
        );
        assert!(prepare_pane_capture_backend(&backend, pane(8, CAPTURE_A), None, false).is_err());
        assert_eq!(
            query_capture_identity_backend(&backend, CAPTURE_A).unwrap(),
            Some(original)
        );
        assert!(query_pane_backend(&backend, 8).unwrap().is_none());
        assert_eq!(
            query_pane_backend(&backend, 7).unwrap().unwrap().pane_uuid.as_deref(),
            Some(CAPTURE_A)
        );
    }

    #[test]
    fn queued_singleton_and_group_writes_keep_their_producer_identity() {
        let backend = backend();
        prepare_pane_capture_backend(&backend, pane(7, CAPTURE_A), Some(native()), false).unwrap();
        let queued = pending(7, CAPTURE_A).into_write_command();
        let queued = pending_append_segment_from_command(queued)
            .unwrap_or_else(|_| panic!("round-trip capture append command"));
        let replacement = MuxPaneIdentity {
            pane_registration: [9; 16],
            ..native()
        };
        prepare_pane_capture_backend(&backend, pane(7, CAPTURE_B), Some(replacement), false)
            .unwrap();

        let singleton = append(&backend, 7, Some(CAPTURE_A)).unwrap();
        let writes = [queued, pending(7, CAPTURE_B)];
        let grouped =
            append_segment_group_commit_backend(&backend, &writes, &mut HashMap::new())
                .unwrap_or_else(|error| panic!("commit queued captures: {error}"));

        for (segment, expected) in [
            (&singleton, CAPTURE_A),
            (&grouped[0].segment, CAPTURE_A),
            (&grouped[1].segment, CAPTURE_B),
        ] {
            assert_eq!(
                query_segment_capture_identity_backend(&backend, segment.id)
                    .unwrap()
                    .unwrap()
                    .capture_uuid,
                expected
            );
        }
        assert_eq!(
            query_pane_backend(&backend, 7).unwrap().unwrap().pane_uuid.as_deref(),
            Some(CAPTURE_B)
        );
    }

    #[test]
    fn missing_or_mismatched_capture_binding_rolls_back_the_append() {
        let backend = backend();
        prepare_pane_capture_backend(&backend, pane(7, CAPTURE_A), Some(native()), false).unwrap();
        prepare_pane_capture_backend(&backend, pane(8, CAPTURE_B), None, false).unwrap();
        for invalid in [CAPTURE_B, CAPTURE_C] {
            assert!(append(&backend, 7, Some(invalid)).is_err());
            assert_eq!(segment_count(&backend), 0);
        }
        let writes = [pending(7, CAPTURE_A), pending(7, CAPTURE_C)];
        assert!(
            append_segment_group_commit_backend(&backend, &writes, &mut HashMap::new()).is_err()
        );
        assert_eq!(segment_count(&backend), 0);
        let valid = append(&backend, 7, Some(CAPTURE_A)).unwrap();
        assert_eq!(valid.seq, 0, "rejected provenance consumes no output sequence");
    }

    #[test]
    fn nil_native_identity_is_rejected_before_metadata_publication() {
        let backend = backend();
        let invalid = MuxPaneIdentity {
            pane_registration: [0; 16],
            ..native()
        };
        assert!(
            prepare_pane_capture_backend(&backend, pane(7, CAPTURE_A), Some(invalid), false)
                .is_err()
        );
        assert!(query_pane_backend(&backend, 7).unwrap().is_none());
        assert!(query_capture_identity_backend(&backend, CAPTURE_A).unwrap().is_none());
    }
}
