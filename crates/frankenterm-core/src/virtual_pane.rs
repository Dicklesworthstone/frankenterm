//! Core adapter for headless command output that should enter ft as a pane.
//!
//! This module is the reusable boundary for `ft virtual run`: callers provide
//! a command spec plus ordered output chunks, and the adapter emits the same
//! distributed wire protocol payloads used by real pane sources.

use serde::{Deserialize, Serialize};

use crate::wire_protocol::{
    PROTOCOL_VERSION, PaneDelta, PaneMeta, WireEnvelope, WirePayload, WireProtocolError,
    validate_sender_identity,
};

pub const VIRTUAL_PANE_DOMAIN: &str = "virtual";

const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtualPaneCommandSpec {
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    pub title: Option<String>,
}

impl VirtualPaneCommandSpec {
    #[must_use]
    pub fn new(argv: Vec<String>) -> Self {
        Self {
            argv,
            cwd: None,
            title: None,
        }
    }

    fn validate(&self) -> Result<(), VirtualPaneError> {
        if self.argv.is_empty() || self.argv.iter().any(|arg| arg.trim().is_empty()) {
            return Err(VirtualPaneError::EmptyCommand);
        }
        Ok(())
    }

    fn display_title(&self) -> String {
        self.title
            .as_ref()
            .filter(|title| !title.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| self.argv.join(" "))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtualPaneOutputChunk {
    pub seq: u64,
    pub content: String,
    pub captured_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtualPaneSessionLog {
    pub sender: String,
    pub command: VirtualPaneCommandSpec,
    pub started_at_ms: i64,
    pub pane_id: Option<u64>,
    pub pane_uuid: Option<String>,
    pub domain: Option<String>,
    pub rows: Option<u16>,
    pub cols: Option<u16>,
    pub chunks: Vec<VirtualPaneOutputChunk>,
}

impl VirtualPaneSessionLog {
    #[must_use]
    pub fn new(
        sender: impl Into<String>,
        command: VirtualPaneCommandSpec,
        started_at_ms: i64,
    ) -> Self {
        Self {
            sender: sender.into(),
            command,
            started_at_ms,
            pane_id: None,
            pane_uuid: None,
            domain: None,
            rows: None,
            cols: None,
            chunks: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtualPaneIngestReceipt {
    pub sender: String,
    pub pane_id: u64,
    pub domain: String,
    pub envelope_count: u64,
    pub delta_count: u64,
    pub byte_count: u64,
    pub first_delta_seq: Option<u64>,
    pub last_delta_seq: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VirtualPaneWireBatch {
    pub envelopes: Vec<WireEnvelope>,
    pub receipt: VirtualPaneIngestReceipt,
}

#[derive(Debug, thiserror::Error)]
pub enum VirtualPaneError {
    #[error("virtual pane command argv must contain at least one non-empty argument")]
    EmptyCommand,
    #[error("virtual pane domain must not be empty")]
    EmptyDomain,
    #[error("virtual pane chunk seq overflow after {last}")]
    SequenceOverflow { last: u64 },
    #[error("virtual pane chunk seq must be contiguous: expected {expected}, got {got}")]
    NonContiguousChunkSeq { expected: u64, got: u64 },
    #[error("virtual pane byte count overflow")]
    ByteCountOverflow,
    #[error("virtual pane envelope/count conversion overflow")]
    CountOverflow,
    #[error(transparent)]
    WireProtocol(#[from] WireProtocolError),
}

pub fn session_log_to_wire_batch(
    log: &VirtualPaneSessionLog,
) -> Result<VirtualPaneWireBatch, VirtualPaneError> {
    validate_sender_identity(&log.sender)?;
    log.command.validate()?;

    let domain = log
        .domain
        .as_deref()
        .unwrap_or(VIRTUAL_PANE_DOMAIN)
        .trim()
        .to_string();
    if domain.is_empty() {
        return Err(VirtualPaneError::EmptyDomain);
    }

    validate_contiguous_chunks(&log.chunks)?;

    let pane_id = log.pane_id.unwrap_or_else(|| deterministic_pane_id(log));
    let mut envelopes = Vec::with_capacity(log.chunks.len().saturating_add(1));
    let mut next_envelope_seq = 0_u64;

    envelopes.push(WireEnvelope {
        version: PROTOCOL_VERSION,
        seq: next_envelope_seq,
        sender: log.sender.clone(),
        sent_at_ms: log.started_at_ms,
        payload: WirePayload::PaneMeta(PaneMeta {
            pane_id,
            pane_uuid: log.pane_uuid.clone(),
            domain: domain.clone(),
            title: Some(log.command.display_title()),
            cwd: log.command.cwd.clone(),
            rows: log.rows,
            cols: log.cols,
            observed: true,
            timestamp_ms: log.started_at_ms,
        }),
    });
    next_envelope_seq = increment_sequence(next_envelope_seq)?;

    let mut byte_count = 0_u64;
    for chunk in &log.chunks {
        let chunk_len =
            u64::try_from(chunk.content.len()).map_err(|_| VirtualPaneError::ByteCountOverflow)?;
        byte_count = byte_count
            .checked_add(chunk_len)
            .ok_or(VirtualPaneError::ByteCountOverflow)?;
        envelopes.push(WireEnvelope {
            version: PROTOCOL_VERSION,
            seq: next_envelope_seq,
            sender: log.sender.clone(),
            sent_at_ms: chunk.captured_at_ms,
            payload: WirePayload::PaneDelta(PaneDelta {
                pane_id,
                seq: chunk.seq,
                content: chunk.content.clone(),
                content_len: chunk.content.len(),
                captured_at_ms: chunk.captured_at_ms,
            }),
        });
        next_envelope_seq = increment_sequence(next_envelope_seq)?;
    }

    Ok(VirtualPaneWireBatch {
        receipt: VirtualPaneIngestReceipt {
            sender: log.sender.clone(),
            pane_id,
            domain,
            envelope_count: u64::try_from(envelopes.len())
                .map_err(|_| VirtualPaneError::CountOverflow)?,
            delta_count: u64::try_from(log.chunks.len())
                .map_err(|_| VirtualPaneError::CountOverflow)?,
            byte_count,
            first_delta_seq: log.chunks.first().map(|chunk| chunk.seq),
            last_delta_seq: log.chunks.last().map(|chunk| chunk.seq),
        },
        envelopes,
    })
}

fn validate_contiguous_chunks(chunks: &[VirtualPaneOutputChunk]) -> Result<(), VirtualPaneError> {
    let Some(first) = chunks.first() else {
        return Ok(());
    };
    let mut expected = first.seq;
    for chunk in chunks {
        if chunk.seq != expected {
            return Err(VirtualPaneError::NonContiguousChunkSeq {
                expected,
                got: chunk.seq,
            });
        }
        expected = increment_sequence(chunk.seq)?;
    }
    Ok(())
}

fn increment_sequence(seq: u64) -> Result<u64, VirtualPaneError> {
    seq.checked_add(1)
        .ok_or(VirtualPaneError::SequenceOverflow { last: seq })
}

fn deterministic_pane_id(log: &VirtualPaneSessionLog) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    hash_str(&mut hash, &log.sender);
    hash_i64(&mut hash, log.started_at_ms);
    if let Some(domain) = &log.domain {
        hash_str(&mut hash, domain);
    }
    if let Some(cwd) = &log.command.cwd {
        hash_str(&mut hash, cwd);
    }
    for arg in &log.command.argv {
        hash_str(&mut hash, arg);
    }
    hash.max(1)
}

fn hash_i64(hash: &mut u64, value: i64) {
    for byte in value.to_le_bytes() {
        hash_byte(hash, byte);
    }
}

fn hash_str(hash: &mut u64, value: &str) {
    for byte in value.as_bytes() {
        hash_byte(hash, *byte);
    }
    hash_byte(hash, 0xff);
}

fn hash_byte(hash: &mut u64, byte: u8) {
    *hash ^= u64::from(byte);
    *hash = hash.wrapping_mul(FNV_PRIME);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    fn sample_log() -> VirtualPaneSessionLog {
        let mut command = VirtualPaneCommandSpec::new(vec![
            "codex".to_string(),
            "exec".to_string(),
            "status".to_string(),
        ]);
        command.cwd = Some("/work/frankenterm".to_string());
        command.title = Some("codex status".to_string());

        let mut log = VirtualPaneSessionLog::new("virtual.host-1", command, 1_800_000_000_000);
        log.rows = Some(24);
        log.cols = Some(80);
        log.chunks = vec![
            VirtualPaneOutputChunk {
                seq: 7,
                content: "starting\n".to_string(),
                captured_at_ms: 1_800_000_000_010,
            },
            VirtualPaneOutputChunk {
                seq: 8,
                content: "done\n".to_string(),
                captured_at_ms: 1_800_000_000_020,
            },
        ];
        log
    }

    #[test]
    fn session_log_emits_pane_meta_then_deltas_on_wire_protocol() -> Result<(), Box<dyn Error>> {
        let batch = session_log_to_wire_batch(&sample_log())?;

        assert_eq!(batch.envelopes.len(), 3);
        assert_eq!(batch.receipt.delta_count, 2);
        assert_eq!(batch.receipt.byte_count, 14);
        assert_eq!(batch.receipt.first_delta_seq, Some(7));
        assert_eq!(batch.receipt.last_delta_seq, Some(8));

        let meta_envelope = batch
            .envelopes
            .first()
            .ok_or("missing pane meta envelope")?;
        match &meta_envelope.payload {
            WirePayload::PaneMeta(meta) => {
                assert_eq!(meta.domain, VIRTUAL_PANE_DOMAIN);
                assert_eq!(meta.title.as_deref(), Some("codex status"));
                assert_eq!(meta.cwd.as_deref(), Some("/work/frankenterm"));
                assert!(meta.observed);
            }
            other => return Err(format!("expected pane meta, got {other:?}").into()),
        }

        let delta_envelope = batch
            .envelopes
            .get(1)
            .ok_or("missing pane delta envelope")?;
        match &delta_envelope.payload {
            WirePayload::PaneDelta(delta) => {
                assert_eq!(delta.seq, 7);
                assert_eq!(delta.content, "starting\n");
                assert_eq!(delta.content_len, "starting\n".len());
            }
            other => return Err(format!("expected pane delta, got {other:?}").into()),
        }

        for envelope in &batch.envelopes {
            let json = envelope.to_json()?;
            WireEnvelope::from_json(&json)?;
        }
        Ok(())
    }

    #[test]
    fn session_log_rejects_non_contiguous_chunk_sequences() -> Result<(), Box<dyn Error>> {
        let mut log = sample_log();
        log.chunks
            .get_mut(1)
            .ok_or("missing second output chunk")?
            .seq = 10;

        let Err(err) = session_log_to_wire_batch(&log) else {
            return Err("gap must fail closed".into());
        };
        assert!(matches!(
            err,
            VirtualPaneError::NonContiguousChunkSeq {
                expected: 8,
                got: 10
            }
        ));
        Ok(())
    }

    #[test]
    fn derived_pane_id_is_stable_for_same_command_identity() -> Result<(), Box<dyn Error>> {
        let first = session_log_to_wire_batch(&sample_log())?;
        let second = session_log_to_wire_batch(&sample_log())?;

        assert_eq!(first.receipt.pane_id, second.receipt.pane_id);
        Ok(())
    }

    #[test]
    fn invalid_sender_is_rejected_before_wire_batch_creation() -> Result<(), Box<dyn Error>> {
        let mut log = sample_log();
        log.sender = "bad sender".to_string();

        let Err(err) = session_log_to_wire_batch(&log) else {
            return Err("sender must be wire-valid".into());
        };
        assert!(matches!(err, VirtualPaneError::WireProtocol(_)));
        Ok(())
    }
}
