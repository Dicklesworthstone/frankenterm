//! Stable witnesses for snapshot checkpoints and restore receipts.
//!
//! The persisted witness is a corruption/consistency check, not an
//! authentication mechanism: a writer that can rewrite the database can also
//! recompute SHA-256. Stable, explicitly framed inputs still let readers
//! distinguish complete, internally consistent v2 records from legacy rows and
//! from accidental partial mutation across toolchain releases.

use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::session_topology::{MAX_SNAPSHOT_BYTES, MAX_TOPOLOGY_PANES};

pub(crate) const SNAPSHOT_DEDUP_PREFIX: &str = "snpd2:";
pub(crate) const SNAPSHOT_WITNESS_PREFIX: &str = "snp2:";
pub(crate) const RESTORE_INTENT_WITNESS_PREFIX: &str = "rsi2:";
pub(crate) const RESTORE_RECEIPT_WITNESS_PREFIX: &str = "rst2:";
pub(crate) const CHECKPOINT_ROLE_SNAPSHOT: &str = "snapshot";
pub(crate) const CHECKPOINT_ROLE_RESTORE_INTENT: &str = "restore_intent";
pub(crate) const CHECKPOINT_ROLE_RESTORE_RECEIPT: &str = "restore_receipt";

/// Maximum UTF-8 bytes stored across the text columns of one
/// `mux_pane_state` row. Scrollback content is stored elsewhere and is not
/// part of this projection.
pub(crate) const MAX_PERSISTED_PANE_TEXT_BYTES: usize = 64 * 1024;
/// Maximum UTF-8 bytes admitted for one checkpoint's topology, metadata, and
/// pane-row text. This is an admission bound, not a claim about SQLite/Rust
/// peak RSS while parsing an already admitted checkpoint.
pub(crate) const MAX_PERSISTED_CHECKPOINT_TEXT_BYTES: usize = 128 * 1024 * 1024;
/// Operator/event metadata is intentionally much smaller than the topology or
/// pane projection. Restore outcome metadata for the maximum pane count fits
/// comfortably within this bound.
pub(crate) const MAX_CHECKPOINT_METADATA_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_CHECKPOINT_SESSION_ID_BYTES: usize = 256;
pub(crate) const MAX_CHECKPOINT_TYPE_BYTES: usize = 64;
/// Maximum stored mux-session build/version identity. Kept with the other
/// persistence projection limits so writers and restore readers share it.
pub(crate) const MAX_SESSION_FT_VERSION_BYTES: usize = 256;
/// Maximum stored optional mux-session host identity.
pub(crate) const MAX_SESSION_HOST_ID_BYTES: usize = 1024;
pub(crate) const MAX_CHECKPOINT_ROLE_BYTES: usize = 32;
pub(crate) const MAX_CHECKPOINT_STATE_HASH_BYTES: usize = 256;

#[derive(Debug, thiserror::Error)]
pub(crate) enum CheckpointWitnessError {
    #[error("checkpoint witness JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("checkpoint witness field is too large: {0}")]
    LengthOverflow(&'static str),
    #[error("checkpoint witness contains duplicate pane id {0}")]
    DuplicatePaneId(i64),
    #[error("checkpoint witness contains an invalid negative value for {0}")]
    NegativeValue(&'static str),
    #[error("checkpoint witness contains an empty value for {0}")]
    EmptyValue(&'static str),
    #[error(
        "checkpoint witness resource limit exceeded for {resource}: observed {observed}, limit {limit}"
    )]
    ResourceLimit {
        resource: &'static str,
        observed: usize,
        limit: usize,
    },
    #[error("unsupported checkpoint role")]
    UnsupportedRole,
}

/// Exact columns persisted for one `mux_pane_state` row.
///
/// JSON strings are canonicalized before a new row is inserted. Readers hash
/// the bytes that are actually stored, so whitespace/key-order rewrites of a
/// v2 row do not silently preserve its witness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PersistedPaneState {
    pub(crate) pane_id: i64,
    pub(crate) cwd: Option<String>,
    pub(crate) command: Option<String>,
    pub(crate) env_json: Option<String>,
    pub(crate) terminal_state_json: String,
    pub(crate) agent_metadata_json: Option<String>,
    pub(crate) scrollback_checkpoint_seq: Option<i64>,
    pub(crate) last_output_at: Option<i64>,
}

/// Exact UTF-8 payload bytes held by the text columns of one persisted pane
/// row. Returns `None` on integer overflow so callers can fail closed.
pub(crate) fn persisted_pane_text_bytes(pane: &PersistedPaneState) -> Option<usize> {
    pane.cwd
        .as_ref()
        .map_or(0, String::len)
        .checked_add(pane.command.as_ref().map_or(0, String::len))?
        .checked_add(pane.env_json.as_ref().map_or(0, String::len))?
        .checked_add(pane.terminal_state_json.len())?
        .checked_add(
            pane.agent_metadata_json
                .as_ref()
                .map_or(0, String::len),
        )
}

/// Exact admitted text bytes for a complete checkpoint projection.
pub(crate) fn persisted_checkpoint_text_bytes(
    topology_json: Option<&str>,
    metadata_json: Option<&str>,
    panes: &[PersistedPaneState],
) -> Option<usize> {
    let mut total = topology_json.map_or(0, str::len);
    total = total.checked_add(metadata_json.map_or(0, str::len))?;
    for pane in panes {
        total = total.checked_add(persisted_pane_text_bytes(pane)?)?;
    }
    Some(total)
}

fn require_resource_limit(
    resource: &'static str,
    observed: usize,
    limit: usize,
) -> Result<(), CheckpointWitnessError> {
    if observed > limit {
        return Err(CheckpointWitnessError::ResourceLimit {
            resource,
            observed,
            limit,
        });
    }
    Ok(())
}

fn validate_persisted_projection(
    topology_json: Option<&str>,
    metadata_json: Option<&str>,
    panes: &[PersistedPaneState],
) -> Result<(), CheckpointWitnessError> {
    require_resource_limit("pane rows", panes.len(), MAX_TOPOLOGY_PANES)?;
    require_resource_limit(
        "topology_json",
        topology_json.map_or(0, str::len),
        MAX_SNAPSHOT_BYTES,
    )?;
    require_resource_limit(
        "metadata_json",
        metadata_json.map_or(0, str::len),
        MAX_CHECKPOINT_METADATA_BYTES,
    )?;
    let mut checkpoint_bytes = topology_json
        .map_or(0, str::len)
        .checked_add(metadata_json.map_or(0, str::len))
        .ok_or(CheckpointWitnessError::LengthOverflow(
            "checkpoint text bytes",
        ))?;

    for pane in panes {
        if pane.pane_id < 0 {
            return Err(CheckpointWitnessError::NegativeValue("pane_id"));
        }
        if pane.scrollback_checkpoint_seq.is_some_and(|value| value < 0) {
            return Err(CheckpointWitnessError::NegativeValue(
                "scrollback_checkpoint_seq",
            ));
        }
        if pane.last_output_at.is_some_and(|value| value < 0) {
            return Err(CheckpointWitnessError::NegativeValue("last_output_at"));
        }
        let pane_bytes = persisted_pane_text_bytes(pane)
            .ok_or(CheckpointWitnessError::LengthOverflow("pane text bytes"))?;
        require_resource_limit(
            "pane text bytes",
            pane_bytes,
            MAX_PERSISTED_PANE_TEXT_BYTES,
        )?;
        checkpoint_bytes = checkpoint_bytes.checked_add(pane_bytes).ok_or(
            CheckpointWitnessError::LengthOverflow("checkpoint text bytes"),
        )?;
        require_resource_limit(
            "checkpoint text bytes",
            checkpoint_bytes,
            MAX_PERSISTED_CHECKPOINT_TEXT_BYTES,
        )?;
    }

    require_resource_limit(
        "checkpoint text bytes",
        checkpoint_bytes,
        MAX_PERSISTED_CHECKPOINT_TEXT_BYTES,
    )
}

/// Recursively sort object keys without changing array order or scalar value.
fn canonicalize_json_value(value: Value) -> Value {
    match value {
        Value::Object(fields) => {
            let mut entries: Vec<(String, Value)> = fields.into_iter().collect();
            entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
            let mut canonical = Map::new();
            for (key, value) in entries {
                canonical.insert(key, canonicalize_json_value(value));
            }
            Value::Object(canonical)
        }
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(canonicalize_json_value)
                .collect(),
        ),
        scalar => scalar,
    }
}

pub(crate) fn canonical_json_string<T: Serialize>(
    value: &T,
) -> Result<String, CheckpointWitnessError> {
    let value = serde_json::to_value(value)?;
    Ok(serde_json::to_string(&canonicalize_json_value(value))?)
}

struct FramedSha256 {
    hasher: Sha256,
}

impl FramedSha256 {
    fn new(domain: &'static [u8]) -> Result<Self, CheckpointWitnessError> {
        let mut framed = Self {
            hasher: Sha256::new(),
        };
        framed.required_bytes("domain", domain)?;
        Ok(framed)
    }

    fn label(&mut self, label: &'static str) -> Result<(), CheckpointWitnessError> {
        let length = u32::try_from(label.len())
            .map_err(|_| CheckpointWitnessError::LengthOverflow("field label"))?;
        self.hasher.update(length.to_be_bytes());
        self.hasher.update(label.as_bytes());
        Ok(())
    }

    fn required_bytes(
        &mut self,
        label: &'static str,
        value: &[u8],
    ) -> Result<(), CheckpointWitnessError> {
        self.label(label)?;
        self.hasher.update([1]);
        let length = u64::try_from(value.len())
            .map_err(|_| CheckpointWitnessError::LengthOverflow(label))?;
        self.hasher.update(length.to_be_bytes());
        self.hasher.update(value);
        Ok(())
    }

    fn optional_bytes(
        &mut self,
        label: &'static str,
        value: Option<&[u8]>,
    ) -> Result<(), CheckpointWitnessError> {
        self.label(label)?;
        match value {
            Some(value) => {
                self.hasher.update([1]);
                let length = u64::try_from(value.len())
                    .map_err(|_| CheckpointWitnessError::LengthOverflow(label))?;
                self.hasher.update(length.to_be_bytes());
                self.hasher.update(value);
            }
            None => self.hasher.update([0]),
        }
        Ok(())
    }

    fn required_i64(
        &mut self,
        label: &'static str,
        value: i64,
    ) -> Result<(), CheckpointWitnessError> {
        self.required_bytes(label, &value.to_be_bytes())
    }

    fn required_u64(
        &mut self,
        label: &'static str,
        value: u64,
    ) -> Result<(), CheckpointWitnessError> {
        self.required_bytes(label, &value.to_be_bytes())
    }

    fn optional_i64(
        &mut self,
        label: &'static str,
        value: Option<i64>,
    ) -> Result<(), CheckpointWitnessError> {
        match value {
            Some(value) => self.optional_bytes(label, Some(&value.to_be_bytes())),
            None => self.optional_bytes(label, None),
        }
    }

    fn finish(self, prefix: &'static str) -> String {
        format!("{prefix}{}", hex::encode(self.hasher.finalize()))
    }
}

fn sorted_panes(
    panes: &[PersistedPaneState],
) -> Result<Vec<&PersistedPaneState>, CheckpointWitnessError> {
    let mut panes: Vec<&PersistedPaneState> = panes.iter().collect();
    panes.sort_unstable_by_key(|pane| pane.pane_id);
    for adjacent in panes.windows(2) {
        if adjacent[0].pane_id == adjacent[1].pane_id {
            return Err(CheckpointWitnessError::DuplicatePaneId(
                adjacent[0].pane_id,
            ));
        }
    }
    Ok(panes)
}

fn frame_panes(
    framed: &mut FramedSha256,
    panes: &[PersistedPaneState],
) -> Result<(), CheckpointWitnessError> {
    framed.required_u64(
        "persisted_pane_row_count",
        u64::try_from(panes.len())
            .map_err(|_| CheckpointWitnessError::LengthOverflow("pane rows"))?,
    )?;

    let mut already_sorted = true;
    for adjacent in panes.windows(2) {
        if adjacent[0].pane_id == adjacent[1].pane_id {
            return Err(CheckpointWitnessError::DuplicatePaneId(
                adjacent[0].pane_id,
            ));
        }
        if adjacent[0].pane_id > adjacent[1].pane_id {
            already_sorted = false;
        }
    }
    if already_sorted {
        for pane in panes {
            frame_pane(framed, pane)?;
        }
        return Ok(());
    }

    for pane in sorted_panes(panes)? {
        frame_pane(framed, pane)?;
    }
    Ok(())
}

fn frame_pane(
    framed: &mut FramedSha256,
    pane: &PersistedPaneState,
) -> Result<(), CheckpointWitnessError> {
    framed.required_i64("pane_id", pane.pane_id)?;
    framed.optional_bytes("cwd", pane.cwd.as_deref().map(str::as_bytes))?;
    framed.optional_bytes("command", pane.command.as_deref().map(str::as_bytes))?;
    framed.optional_bytes(
        "env_json",
        pane.env_json.as_deref().map(str::as_bytes),
    )?;
    framed.required_bytes(
        "terminal_state_json",
        pane.terminal_state_json.as_bytes(),
    )?;
    framed.optional_bytes(
        "agent_metadata_json",
        pane.agent_metadata_json.as_deref().map(str::as_bytes),
    )?;
    framed.optional_i64(
        "scrollback_checkpoint_seq",
        pane.scrollback_checkpoint_seq,
    )?;
    framed.optional_i64("last_output_at", pane.last_output_at)?;
    Ok(())
}

/// Stable semantic digest used only for in-memory periodic deduplication.
///
/// The topology capture clock is excluded. Every other input is an exact
/// column that the checkpoint transaction persists; process PID/argv, shell,
/// and scrollback total-segment counters therefore do not participate.
pub(crate) fn snapshot_dedup_witness(
    topology_json: &str,
    panes: &[PersistedPaneState],
) -> Result<String, CheckpointWitnessError> {
    validate_persisted_projection(Some(topology_json), None, panes)?;
    let mut topology: Value = serde_json::from_str(topology_json)?;
    if let Value::Object(fields) = &mut topology {
        fields.remove("captured_at");
    }
    let topology = serde_json::to_string(&canonicalize_json_value(topology))?;

    let mut framed = FramedSha256::new(b"frankenterm:snapshot-dedup:v2")?;
    framed.required_bytes("topology_json", topology.as_bytes())?;
    frame_panes(&mut framed, panes)?;
    Ok(framed.finish(SNAPSHOT_DEDUP_PREFIX))
}

/// Stable persisted checkpoint witness for a snapshot, restore intent, or
/// restore outcome receipt.
#[allow(clippy::too_many_arguments)]
pub(crate) fn checkpoint_witness(
    role: &str,
    session_id: &str,
    checkpoint_id: i64,
    checkpoint_at: i64,
    checkpoint_type: &str,
    pane_count: i64,
    total_bytes: i64,
    metadata_json: Option<&str>,
    topology_json: Option<&str>,
    panes: &[PersistedPaneState],
) -> Result<String, CheckpointWitnessError> {
    require_resource_limit(
        "checkpoint_role",
        role.len(),
        MAX_CHECKPOINT_ROLE_BYTES,
    )?;
    // Reject an unsupported role before inspecting any pane payload. The role
    // selects the hash domain, so no other projection work is useful until it
    // is known to be valid.
    let (domain, prefix) = match role {
        CHECKPOINT_ROLE_SNAPSHOT => (
            &b"frankenterm:snapshot-checkpoint:v2"[..],
            SNAPSHOT_WITNESS_PREFIX,
        ),
        CHECKPOINT_ROLE_RESTORE_INTENT => (
            &b"frankenterm:restore-intent:v2"[..],
            RESTORE_INTENT_WITNESS_PREFIX,
        ),
        CHECKPOINT_ROLE_RESTORE_RECEIPT => (
            &b"frankenterm:restore-receipt:v2"[..],
            RESTORE_RECEIPT_WITNESS_PREFIX,
        ),
        _ => return Err(CheckpointWitnessError::UnsupportedRole),
    };

    require_resource_limit(
        "session_id",
        session_id.len(),
        MAX_CHECKPOINT_SESSION_ID_BYTES,
    )?;
    require_resource_limit(
        "checkpoint_type",
        checkpoint_type.len(),
        MAX_CHECKPOINT_TYPE_BYTES,
    )?;
    if session_id.is_empty() {
        return Err(CheckpointWitnessError::EmptyValue("session_id"));
    }
    if checkpoint_type.is_empty() {
        return Err(CheckpointWitnessError::EmptyValue("checkpoint_type"));
    }
    for (field, value) in [
        ("checkpoint_id", checkpoint_id),
        ("checkpoint_at", checkpoint_at),
        ("pane_count", pane_count),
        ("total_bytes", total_bytes),
    ] {
        if value < 0 {
            return Err(CheckpointWitnessError::NegativeValue(field));
        }
    }
    let declared_pane_count = usize::try_from(pane_count)
        .map_err(|_| CheckpointWitnessError::LengthOverflow("pane_count"))?;
    require_resource_limit(
        "pane_count",
        declared_pane_count,
        MAX_TOPOLOGY_PANES,
    )?;
    let declared_total_bytes = usize::try_from(total_bytes)
        .map_err(|_| CheckpointWitnessError::LengthOverflow("total_bytes"))?;
    require_resource_limit(
        "total_bytes",
        declared_total_bytes,
        MAX_PERSISTED_CHECKPOINT_TEXT_BYTES,
    )?;
    validate_persisted_projection(topology_json, metadata_json, panes)?;

    let mut framed = FramedSha256::new(domain)?;
    framed.required_bytes("checkpoint_role", role.as_bytes())?;
    framed.required_bytes("session_id", session_id.as_bytes())?;
    framed.required_i64("checkpoint_id", checkpoint_id)?;
    framed.required_i64("checkpoint_at", checkpoint_at)?;
    framed.required_bytes("checkpoint_type", checkpoint_type.as_bytes())?;
    framed.required_i64("pane_count", pane_count)?;
    framed.required_i64("total_bytes", total_bytes)?;
    framed.optional_bytes("metadata_json", metadata_json.map(str::as_bytes))?;
    framed.optional_bytes("topology_json", topology_json.map(str::as_bytes))?;
    frame_panes(&mut framed, panes)?;
    Ok(framed.finish(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_topology(captured_at: u64) -> String {
        format!(
            "{{\"captured_at\":{captured_at},\"schema_version\":1,\"windows\":[],\"workspace_id\":null}}"
        )
    }

    #[test]
    fn canonical_json_sorts_nested_objects_without_reordering_arrays() {
        let value = serde_json::json!({
            "z": {"b": 2, "a": 1},
            "a": [{"d": 4, "c": 3}, 2, 1],
        });
        assert_eq!(
            canonical_json_string(&value).unwrap(),
            r#"{"a":[{"c":3,"d":4},2,1],"z":{"a":1,"b":2}}"#
        );
    }

    #[test]
    fn snapshot_dedup_excludes_only_topology_capture_clock() {
        let first = snapshot_dedup_witness(&empty_topology(1), &[]).unwrap();
        let second = snapshot_dedup_witness(&empty_topology(99), &[]).unwrap();
        assert_eq!(first, second);
        assert!(first.starts_with(SNAPSHOT_DEDUP_PREFIX));
    }

    #[test]
    fn witness_role_domains_are_distinct() {
        let snapshot = checkpoint_witness(
            CHECKPOINT_ROLE_SNAPSHOT,
            "sess",
            7,
            11,
            "startup",
            0,
            0,
            None,
            Some(&empty_topology(11)),
            &[],
        )
        .unwrap();
        let receipt = checkpoint_witness(
            CHECKPOINT_ROLE_RESTORE_RECEIPT,
            "sess",
            7,
            11,
            "startup",
            0,
            0,
            None,
            None,
            &[],
        )
        .unwrap();
        let intent = checkpoint_witness(
            CHECKPOINT_ROLE_RESTORE_INTENT,
            "sess",
            7,
            11,
            "startup",
            0,
            0,
            None,
            None,
            &[],
        )
        .unwrap();
        assert!(snapshot.starts_with(SNAPSHOT_WITNESS_PREFIX));
        assert!(intent.starts_with(RESTORE_INTENT_WITNESS_PREFIX));
        assert!(receipt.starts_with(RESTORE_RECEIPT_WITNESS_PREFIX));
        assert_ne!(snapshot, receipt);
        assert_ne!(snapshot, intent);
        assert_ne!(intent, receipt);
    }

    #[test]
    fn fixed_empty_vectors_pin_cross_release_framing() {
        let topology = empty_topology(11);
        assert_eq!(
            snapshot_dedup_witness(&topology, &[]).unwrap(),
            "snpd2:e6467ddc380a22d7198585a511ec35ae988c68d092714e0fc4b3895534feedac"
        );
        assert_eq!(
            checkpoint_witness(
                CHECKPOINT_ROLE_SNAPSHOT,
                "sess",
                7,
                11,
                "startup",
                0,
                0,
                None,
                Some(&topology),
                &[],
            )
            .unwrap(),
            "snp2:1143f1e341b53fa623bddf79e3e963d7d98649dd3e145b35d5231a9414d460f1"
        );
        assert_eq!(
            checkpoint_witness(
                CHECKPOINT_ROLE_RESTORE_RECEIPT,
                "sess",
                7,
                11,
                "startup",
                0,
                0,
                None,
                None,
                &[],
            )
            .unwrap(),
            "rst2:efe2b0ede649a2f4f00d2b8873b09cc589cd23b0867c1711529215882142d07f"
        );
    }

    #[test]
    fn fixed_populated_vector_pins_optional_utf8_and_i64_framing() {
        let topology = empty_topology(1234);
        let pane = PersistedPaneState {
            pane_id: 42,
            cwd: Some("/tmp/☃".to_string()),
            command: Some("zsh".to_string()),
            env_json: Some(r#"{"vars":{"LANG":"C"}}"#.to_string()),
            terminal_state_json: r#"{"cols":80,"rows":24}"#.to_string(),
            agent_metadata_json: None,
            scrollback_checkpoint_seq: Some(9),
            last_output_at: Some(1234),
        };
        assert_eq!(
            checkpoint_witness(
                CHECKPOINT_ROLE_SNAPSHOT,
                "sess-unicode",
                99,
                1234,
                "event",
                1,
                123,
                Some(r#"{"reason":"golden"}"#),
                Some(&topology),
                &[pane],
            )
            .unwrap(),
            "snp2:a32d1f49065f8e7c64aa64e3fe2f6b508bec43ea406c02b690d2417dcfbaa1a1"
        );
    }

    #[test]
    fn pane_order_is_canonical_but_duplicates_and_optional_shape_are_distinct() {
        let topology = empty_topology(1);
        let pane = |pane_id, cwd| PersistedPaneState {
            pane_id,
            cwd,
            command: None,
            env_json: None,
            terminal_state_json: "{}".to_string(),
            agent_metadata_json: None,
            scrollback_checkpoint_seq: None,
            last_output_at: None,
        };
        let first = pane(1, None);
        let second = pane(2, Some(String::new()));
        let ordered = snapshot_dedup_witness(&topology, &[first.clone(), second.clone()]).unwrap();
        let reversed = snapshot_dedup_witness(&topology, &[second.clone(), first.clone()]).unwrap();
        assert_eq!(ordered, reversed);

        let empty_instead_of_none =
            snapshot_dedup_witness(&topology, &[pane(1, Some(String::new())), second.clone()])
                .unwrap();
        assert_ne!(ordered, empty_instead_of_none);

        assert!(matches!(
            snapshot_dedup_witness(&topology, &[first.clone(), first]),
            Err(CheckpointWitnessError::DuplicatePaneId(1))
        ));
    }

    #[test]
    fn persisted_text_accounting_uses_utf8_bytes_and_all_projected_columns() {
        let pane = PersistedPaneState {
            pane_id: 1,
            cwd: Some("☃".to_owned()),
            command: Some("zsh".to_owned()),
            env_json: Some("{}".to_owned()),
            terminal_state_json: "[]".to_owned(),
            agent_metadata_json: Some("null".to_owned()),
            scrollback_checkpoint_seq: None,
            last_output_at: None,
        };
        assert_eq!(persisted_pane_text_bytes(&pane), Some(14));
        assert_eq!(
            persisted_checkpoint_text_bytes(Some("{}"), Some("[]"), &[pane]),
            Some(18)
        );
    }

    #[test]
    fn witness_rejects_oversized_pane_projection_before_hashing() {
        let pane = PersistedPaneState {
            pane_id: 1,
            cwd: None,
            command: None,
            env_json: None,
            terminal_state_json: "x".repeat(MAX_PERSISTED_PANE_TEXT_BYTES + 1),
            agent_metadata_json: None,
            scrollback_checkpoint_seq: None,
            last_output_at: None,
        };
        assert!(matches!(
            snapshot_dedup_witness(&empty_topology(1), &[pane]),
            Err(CheckpointWitnessError::ResourceLimit {
                resource: "pane text bytes",
                observed,
                limit: MAX_PERSISTED_PANE_TEXT_BYTES,
            }) if observed == MAX_PERSISTED_PANE_TEXT_BYTES + 1
        ));
    }

    #[test]
    fn unsupported_role_error_does_not_echo_untrusted_role() {
        let role = "credential-canary";
        let error = checkpoint_witness(role, "sess", 1, 1, "startup", 0, 0, None, None, &[])
            .expect_err("unsupported role must fail closed");
        let rendered = error.to_string();
        assert!(matches!(error, CheckpointWitnessError::UnsupportedRole));
        assert!(!rendered.contains(role));
    }

    #[test]
    fn checkpoint_witness_rejects_each_negative_checkpoint_scalar_exactly() {
        let cases = [
            ("checkpoint_id", -1, 1, 0, 0),
            ("checkpoint_at", 1, -1, 0, 0),
            ("pane_count", 1, 1, -1, 0),
            ("total_bytes", 1, 1, 0, -1),
        ];

        for (expected_field, checkpoint_id, checkpoint_at, pane_count, total_bytes) in cases {
            let error = checkpoint_witness(
                CHECKPOINT_ROLE_RESTORE_RECEIPT,
                "sess",
                checkpoint_id,
                checkpoint_at,
                "startup",
                pane_count,
                total_bytes,
                None,
                None,
                &[],
            )
            .expect_err("negative checkpoint scalar must fail closed");
            assert!(
                matches!(
                    error,
                    CheckpointWitnessError::NegativeValue(field) if field == expected_field
                ),
                "expected exact negative-value field {expected_field}"
            );
        }
    }

    #[test]
    fn checkpoint_witness_rejects_each_negative_pane_scalar_exactly() {
        let cases = [
            ("pane_id", -1, Some(0), Some(0)),
            ("scrollback_checkpoint_seq", 1, Some(-1), Some(0)),
            ("last_output_at", 1, Some(0), Some(-1)),
        ];

        for (expected_field, pane_id, scrollback_checkpoint_seq, last_output_at) in cases {
            let pane = PersistedPaneState {
                pane_id,
                cwd: None,
                command: None,
                env_json: None,
                terminal_state_json: "{}".to_owned(),
                agent_metadata_json: None,
                scrollback_checkpoint_seq,
                last_output_at,
            };
            let topology = empty_topology(1);
            let error = checkpoint_witness(
                CHECKPOINT_ROLE_SNAPSHOT,
                "sess",
                1,
                1,
                "periodic",
                1,
                2,
                None,
                Some(&topology),
                &[pane],
            )
            .expect_err("negative pane scalar must fail closed");
            assert!(
                matches!(
                    error,
                    CheckpointWitnessError::NegativeValue(field) if field == expected_field
                ),
                "expected exact negative-value field {expected_field}"
            );
        }
    }

    #[test]
    fn witness_rejects_fields_that_the_bounded_reader_cannot_admit() {
        assert!(matches!(
            checkpoint_witness(
                CHECKPOINT_ROLE_SNAPSHOT,
                "",
                1,
                1,
                "periodic",
                0,
                0,
                None,
                Some(&empty_topology(1)),
                &[],
            ),
            Err(CheckpointWitnessError::EmptyValue("session_id"))
        ));
        assert!(matches!(
            checkpoint_witness(
                CHECKPOINT_ROLE_SNAPSHOT,
                "sess",
                1,
                1,
                "",
                0,
                0,
                None,
                Some(&empty_topology(1)),
                &[],
            ),
            Err(CheckpointWitnessError::EmptyValue("checkpoint_type"))
        ));
        assert!(matches!(
            checkpoint_witness(
                CHECKPOINT_ROLE_RESTORE_RECEIPT,
                "sess",
                1,
                1,
                "startup",
                i64::try_from(MAX_TOPOLOGY_PANES).unwrap() + 1,
                0,
                None,
                None,
                &[],
            ),
            Err(CheckpointWitnessError::ResourceLimit {
                resource: "pane_count",
                observed,
                limit: MAX_TOPOLOGY_PANES,
            }) if observed == MAX_TOPOLOGY_PANES + 1
        ));
        assert!(matches!(
            checkpoint_witness(
                CHECKPOINT_ROLE_SNAPSHOT,
                "sess",
                1,
                1,
                "periodic",
                0,
                i64::try_from(MAX_PERSISTED_CHECKPOINT_TEXT_BYTES).unwrap() + 1,
                None,
                Some(&empty_topology(1)),
                &[],
            ),
            Err(CheckpointWitnessError::ResourceLimit {
                resource: "total_bytes",
                observed,
                limit: MAX_PERSISTED_CHECKPOINT_TEXT_BYTES,
            }) if observed == MAX_PERSISTED_CHECKPOINT_TEXT_BYTES + 1
        ));
    }
}
