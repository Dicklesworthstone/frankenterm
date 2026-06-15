//! Comprehensive secret-redactor conformance e2e (ft-gfbfw).
//!
//! Exercises every one of the 32 `SECRET_PATTERNS` catalog entries across:
//!   1. batch `Redactor::redact` — the pattern is detected and the secret value
//!      is removed (NO false-negatives);
//!   2. `StreamingRedactor` fed the secret split at EVERY byte boundary — the
//!      secret never leaks across chunk boundaries (the ft-b1p6x split-anchor
//!      class of bug);
//!   3. multibyte-UTF-8 boundaries — secrets flanked by `é`/emoji, streamed with
//!      splits landing mid-codepoint, never panic and still redact;
//!   4. raw / invalid-UTF-8 bytes — `redact_bytes_with_evidence` and the
//!      streaming path never panic and never leak.
//!
//! Zero-RCH authored; runs in-process against the live catalog.

use frankenterm_core::redactor::{Redactor, StreamingRedactor};

/// Alphanumeric leak sentinel (accepted by the alnum / base64 / `_-` charsets).
const S: &str = "LEAKDETECT";
/// Hex-charset twin of `S`, for the two hex-only patterns (twilio / datadog).
const HX: &str = "decade1234";

fn z(n: usize) -> String {
    "0".repeat(n)
}

/// `(pattern name, a regex-valid sample, the distinctive secret substring that
/// MUST be absent from any redacted output)`. Bodies are built with explicit
/// `z(n)` padding generously past each regex length floor — no hand-counted
/// literals. Leading spaces on `generic_token` / `generic_secret` satisfy the
/// `(?:^|[^a-z])` boundary those patterns require.
fn catalog() -> Vec<(&'static str, String, &'static str)> {
    vec![
        ("anthropic_key", format!("sk-ant-{S}{}", z(40)), S),
        ("openai_key", format!("sk-proj-{S}{}", z(16)), S),
        ("github_token", format!("ghp_{S}{}", z(36)), S),
        ("github_fine_grained_pat", format!("github_pat_{S}{}", z(50)), S),
        ("gitlab_token", format!("glpat-{S}{}", z(20)), S),
        ("xai_key", format!("xai-{S}{}", z(40)), S),
        ("groq_key", format!("gsk_{S}{}", z(40)), S),
        ("google_api_key", format!("AIza{S}{}", z(35)), S),
        ("google_oauth_token", format!("ya29.{S}{}", z(20)), S),
        ("huggingface_token", format!("hf_{S}{}", z(30)), S),
        ("replicate_token", format!("r8_{S}{}", z(30)), S),
        ("anyscale_key", format!("esecret_{S}{}", z(30)), S),
        ("perplexity_key", format!("pplx-{S}{}", z(40)), S),
        ("ai_provider_keyed_value", format!("cohere_api_key={S}{}", z(16)), S),
        // AKIA charset is uppercase+digits; `S` is all-uppercase, `z()` digits.
        ("aws_access_key_id", format!("AKIA{S}{}", z(16)), S),
        ("aws_secret_key", format!("aws_secret_access_key={S}{}", z(40)), S),
        ("bearer_token", format!("Bearer {S}{}", z(20)), S),
        ("slack_token", format!("xoxb-{S}{}", z(10)), S),
        ("stripe_key", format!("sk_live_{S}{}", z(20)), S),
        ("twilio_account_sid", format!("AC{HX}{}", z(32)), HX),
        ("sendgrid_key", format!("SG.{S}{}.{}{}", z(20), S, z(40)), S),
        ("datadog_api_key", format!("DD_API_KEY={HX}{}", z(32)), HX),
        ("database_url", format!("postgres://user:{S}pw@dbhost:5432/app"), S),
        (
            "ssh_private_key",
            format!("-----BEGIN OPENSSH PRIVATE KEY-----\n{S}bodyline\n-----END OPENSSH PRIVATE KEY-----"),
            S,
        ),
        (
            "pgp_block",
            format!("-----BEGIN PGP PRIVATE KEY BLOCK-----\n{S}bodyline\n-----END PGP PRIVATE KEY BLOCK-----"),
            S,
        ),
        ("jwt_token", format!("eyJ{S}aa.eyJ0{S}bb.sig{S}cc"), S),
        ("device_code", format!("device_code={S}"), S),
        ("oauth_url", format!("https://app.example/cb#access_token={S}tok"), S),
        ("generic_api_key", format!("api_key={S}{}", z(16)), S),
        ("generic_token", format!(" token={S}{}", z(16)), S),
        ("generic_password", format!("password={S}pw"), S),
        ("generic_secret", format!(" secret={S}{}", z(8)), S),
    ]
}

#[test]
fn catalog_has_all_32_patterns() {
    assert_eq!(catalog().len(), 32, "catalog table must cover all 32 patterns");
}

/// Batch redaction: every sample is detected as a secret and its value is
/// scrubbed — no false-negatives.
#[test]
fn batch_redact_removes_every_catalog_secret() {
    let redactor = Redactor::new();
    for (name, sample, must_be_absent) in catalog() {
        let text = format!("log >> {sample} << end");

        assert!(
            !redactor.detect(&text).is_empty(),
            "[{name}] sample must be detected as a secret: {text:?}"
        );

        let redacted = redactor.redact(&text);
        assert_ne!(redacted, text, "[{name}] redact must change the text");
        assert!(
            !redacted.contains(must_be_absent),
            "[{name}] FALSE-NEGATIVE: '{must_be_absent}' survived redaction: {redacted:?}"
        );
        assert!(
            redacted.contains("[REDACTED]"),
            "[{name}] redacted output must carry the marker: {redacted:?}"
        );
    }
}

/// Feed `text` through the streaming redactor split at one byte boundary and
/// return the concatenated emitted bytes (chunk1 + chunk2 + finish).
fn streamed_output(text: &str, split: usize) -> Vec<u8> {
    let mut streaming = StreamingRedactor::new();
    let bytes = text.as_bytes();
    let mut out = streaming.redact_chunk(&bytes[..split]).bytes;
    out.extend(streaming.redact_chunk(&bytes[split..]).bytes);
    out.extend(streaming.finish().bytes);
    out
}

#[test]
fn streaming_never_leaks_a_secret_across_any_split_boundary() {
    for (name, sample, must_be_absent) in catalog() {
        let text = format!("head|{sample}|tail");
        for split in 1..text.len() {
            let out = streamed_output(&text, split);
            let rendered = String::from_utf8_lossy(&out);
            assert!(
                !rendered.contains(must_be_absent),
                "[{name}] LEAK at split={split}: '{must_be_absent}' in {rendered:?}"
            );
        }
    }
}

/// Single-chunk streaming must also fully redact (parity at the whole-input
/// boundary).
#[test]
fn streaming_single_chunk_redacts_every_secret() {
    for (name, sample, must_be_absent) in catalog() {
        let text = format!("head|{sample}|tail");
        let mut streaming = StreamingRedactor::new();
        let mut out = streaming.redact_chunk(text.as_bytes()).bytes;
        out.extend(streaming.finish().bytes);
        let rendered = String::from_utf8_lossy(&out);
        assert!(
            !rendered.contains(must_be_absent),
            "[{name}] single-chunk leak: {rendered:?}"
        );
    }
}

/// Secrets flanked by multibyte UTF-8, streamed with splits that land mid
/// codepoint, must never panic and must still redact.
#[test]
fn multibyte_utf8_boundaries_never_panic_and_still_redact() {
    for (name, sample, must_be_absent) in catalog() {
        // 'é' is 2 bytes, '🚀' is 4 bytes — splits will land inside them.
        let text = format!("préfix🚀{sample}🚀suffixé");
        for split in 1..text.len() {
            let out = streamed_output(&text, split);
            let rendered = String::from_utf8_lossy(&out);
            assert!(
                !rendered.contains(must_be_absent),
                "[{name}] multibyte leak at split={split}: {rendered:?}"
            );
        }
        let redacted = Redactor::new().redact(&text);
        assert!(
            !redacted.contains(must_be_absent),
            "[{name}] multibyte batch leak: {redacted:?}"
        );
    }
}

/// Raw / invalid-UTF-8 byte streams must never panic and must not leak the
/// secret, whether redacted whole or split at every boundary.
#[test]
fn raw_invalid_utf8_bytes_never_panic_or_leak() {
    let redactor = Redactor::new();
    for (name, sample, must_be_absent) in catalog() {
        // Wrap the secret in invalid-UTF-8 noise (lone continuation/lead bytes).
        let mut bytes = vec![0xFF, 0xFE, 0x80, 0xC0];
        bytes.extend_from_slice(sample.as_bytes());
        bytes.extend_from_slice(&[0x80, 0xFF, 0xF8]);

        let result = redactor.redact_bytes_with_evidence(&bytes);
        let rendered = String::from_utf8_lossy(&result.bytes);
        assert!(
            !rendered.contains(must_be_absent),
            "[{name}] raw-byte batch leak: {rendered:?}"
        );

        for split in 1..bytes.len() {
            let mut streaming = StreamingRedactor::new();
            let mut out = streaming.redact_chunk(&bytes[..split]).bytes;
            out.extend(streaming.redact_chunk(&bytes[split..]).bytes);
            out.extend(streaming.finish().bytes);
            let rendered = String::from_utf8_lossy(&out);
            assert!(
                !rendered.contains(must_be_absent),
                "[{name}] raw-byte stream leak at split={split}: {rendered:?}"
            );
        }
    }
}
