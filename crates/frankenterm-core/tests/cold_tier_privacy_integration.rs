use frankenterm_core::redactor::{REDACTED_MARKER, Redactor};
use frankenterm_core::scrollback_cold_tier_pipeline::{
    ChunkBytes, ColdTierKeyHandle, PipelineHealth, Raw, RedactionEvidence, Written,
};

#[test]
fn redactor_evidence_drives_cold_tier_write_privacy_health() {
    let secret = "sk-proj-abcdefghijklmnopqrstuvwxyz123456";
    let raw_line = format!("export OPENAI_API_KEY={secret}\n");
    let raw_len = raw_line.len() as u32;
    let redactor = Redactor::new();

    let raw = ChunkBytes::<Raw>::from_raw(raw_line.into_bytes());
    let (redacted, evidence) = raw.redact_with_evidence(|bytes| {
        let result = redactor.redact_bytes_with_evidence(&bytes);
        let evidence = RedactionEvidence {
            matches: result.evidence.matches,
            bytes_replaced: result.evidence.bytes_replaced,
        };
        (result.bytes, evidence)
    });

    let redacted_text =
        String::from_utf8(redacted.as_bytes().to_vec()).expect("redactor emits utf-8");
    assert!(evidence.redactor_applied());
    assert!(evidence.made_changes());
    assert!(!redacted_text.contains(secret));
    assert!(redacted_text.contains(REDACTED_MARKER));

    let compressed = redacted.compress_with(|bytes| bytes);
    let key = ColdTierKeyHandle {
        key_id: "test-key".to_string(),
        mmap_key_slug: "test-mmap-key".to_string(),
    };
    let encrypted = compressed.encrypt_with(&key, |bytes| bytes);

    let mut written_bytes = Vec::new();
    let written: ChunkBytes<Written> = encrypted
        .write_with::<_, ()>(|bytes| {
            written_bytes.extend_from_slice(bytes);
            Ok(())
        })
        .expect("test writer succeeds");

    assert_eq!(written.as_bytes(), written_bytes.as_slice());
    assert!(!String::from_utf8_lossy(&written_bytes).contains(secret));

    let mut health = PipelineHealth::baseline();
    health.record_write(
        raw_len,
        written.len() as u32,
        evidence.redactor_applied(),
        true,
    );

    assert!(health.is_safe());
    assert_eq!(health.chunks_written_total, 1);
    assert_eq!(health.redactions_applied_total, 1);
    assert_eq!(health.chunks_written_without_redactor, 0);
    assert_eq!(health.encryptions_applied_total, 1);
}
