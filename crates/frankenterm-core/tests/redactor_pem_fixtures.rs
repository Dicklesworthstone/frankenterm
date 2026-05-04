//! End-to-end fixture coverage for SSH/PEM and PGP/OpenPGP
//! armoured-block redaction (br-ft-2xkrc).
//!
//! Sister to `proptest_redactor_conformance.rs` (structural
//! invariants) — this file pins **realistic-shape** fixtures for
//! every BEGIN/END envelope variant, plus the
//! pasted-into-a-pane mixed-content scenarios that operators are
//! most likely to trigger by accident:
//!
//! - `cat ~/.ssh/id_rsa` (RSA key body in a terminal)
//! - `cat ~/.ssh/id_ed25519` (OpenSSH new-format → ED25519 body)
//! - `gpg --export-secret-key user@example.com` (PGP private)
//! - PGP signed message in a code-review snippet
//! - Multiple keys in one paste (independent redactions)
//!
//! Fixtures use synthetic bodies (clearly labelled "EXAMPLE",
//! "NOT_A_REAL_KEY") so an accidental leak in the test artefacts
//! cannot be mistaken for a real credential.

use frankenterm_core::redactor::Redactor;

const REDACTED: &str = "[REDACTED]";

// ---------------------------------------------------------------------------
// SSH / OpenSSL PEM key fixtures
// ---------------------------------------------------------------------------

const SSH_RSA: &str = "-----BEGIN RSA PRIVATE KEY-----
MIIEpAIBAAKCAQEA_EXAMPLE_FIXTURE_NOT_A_REAL_KEY_aBcDeFgHiJkLmNoPq
RsTuVwXyZ1234567890aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890aBcDeFgHi
-----END RSA PRIVATE KEY-----";

const SSH_DSA: &str = "-----BEGIN DSA PRIVATE KEY-----
MIIBugIBAAKBgQDexample_dsa_body_aBcDeFgHiJkLmNoPqRsTuVwXyZ12345
-----END DSA PRIVATE KEY-----";

const SSH_EC: &str = "-----BEGIN EC PRIVATE KEY-----
MHcCAQEEIH9example_ec_body_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890
-----END EC PRIVATE KEY-----";

const SSH_OPENSSH: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtz
c2gtZWQyNTUxOQAAACBOEXAMPLE_OPENSSH_BODY_NOT_A_REAL_KEY
-----END OPENSSH PRIVATE KEY-----";

const SSH_ED25519_NONSTANDARD: &str = "-----BEGIN ED25519 PRIVATE KEY-----
EXAMPLE_ED25519_BODY_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890_NOT_REAL
-----END ED25519 PRIVATE KEY-----";

const PKCS8_UNENCRYPTED: &str = "-----BEGIN PRIVATE KEY-----
MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQDB_EXAMPLE_PKCS8
-----END PRIVATE KEY-----";

const PKCS8_ENCRYPTED: &str = "-----BEGIN ENCRYPTED PRIVATE KEY-----
MIIE6TAbBgkqhkiG9w0BBQMwDgQI_EXAMPLE_ENC_PKCS8_NOT_A_REAL_KEY
-----END ENCRYPTED PRIVATE KEY-----";

// ---------------------------------------------------------------------------
// PGP / OpenPGP armoured-block fixtures
// ---------------------------------------------------------------------------

const PGP_PRIVATE: &str = "-----BEGIN PGP PRIVATE KEY BLOCK-----

lQOYBEXAMPLE_PGP_PRIVATE_BODY_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890
=Y0Yw
-----END PGP PRIVATE KEY BLOCK-----";

const PGP_PUBLIC: &str = "-----BEGIN PGP PUBLIC KEY BLOCK-----

mQENBEXAMPLE_PGP_PUBLIC_BODY_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890
=A1B2
-----END PGP PUBLIC KEY BLOCK-----";

const PGP_MESSAGE: &str = "-----BEGIN PGP MESSAGE-----

hQEMA0EXAMPLE_PGP_ENCRYPTED_BODY_aBcDeFgHiJkLmNoPqRsTuVwXyZ12345
=zXyZ
-----END PGP MESSAGE-----";

const PGP_SIGNED: &str = "-----BEGIN PGP SIGNED MESSAGE-----
Hash: SHA256

This is a signed payload.
-----BEGIN PGP SIGNATURE-----

iQEzBAEBCAAdFEXAMPLE_PGP_SIGNATURE_BODY_aBcDeFgHiJkLmNoPq
=qWEr
-----END PGP SIGNATURE-----";

// ---------------------------------------------------------------------------
// Per-fixture redaction tests
// ---------------------------------------------------------------------------

fn assert_fully_scrubbed(label: &str, fixture: &str, body_marker: &str) {
    let r = Redactor::new();
    let envelope = format!("operator pasted:\n{fixture}\nrest of line");
    let out = r.redact(&envelope);
    assert!(
        !out.contains(body_marker),
        "ft-2xkrc: {label} body marker `{body_marker}` leaked through redact: {out:?}"
    );
    assert!(
        out.contains(REDACTED),
        "ft-2xkrc: {label} produced no [REDACTED] marker: {out:?}"
    );
    assert!(
        !r.contains_secrets(&out),
        "ft-2xkrc: {label} left a residue contains_secrets caught: {out:?}"
    );
}

#[test]
fn fixture_ssh_rsa_private_key() {
    assert_fully_scrubbed("SSH RSA", SSH_RSA, "EXAMPLE_FIXTURE");
}

#[test]
fn fixture_ssh_dsa_private_key() {
    assert_fully_scrubbed("SSH DSA", SSH_DSA, "example_dsa_body");
}

#[test]
fn fixture_ssh_ec_private_key() {
    assert_fully_scrubbed("SSH EC", SSH_EC, "example_ec_body");
}

#[test]
fn fixture_ssh_openssh_new_format() {
    assert_fully_scrubbed("OpenSSH new-format", SSH_OPENSSH, "EXAMPLE_OPENSSH_BODY");
}

#[test]
fn fixture_ssh_ed25519_nonstandard_header() {
    // br-ft-2xkrc: digit-bearing algo prefix.
    assert_fully_scrubbed(
        "SSH ED25519 (nonstandard header)",
        SSH_ED25519_NONSTANDARD,
        "EXAMPLE_ED25519_BODY",
    );
}

#[test]
fn fixture_pkcs8_unencrypted() {
    assert_fully_scrubbed("PKCS#8 unencrypted", PKCS8_UNENCRYPTED, "EXAMPLE_PKCS8");
}

#[test]
fn fixture_pkcs8_encrypted() {
    assert_fully_scrubbed("PKCS#8 encrypted", PKCS8_ENCRYPTED, "EXAMPLE_ENC_PKCS8");
}

#[test]
fn fixture_pgp_private_block() {
    assert_fully_scrubbed("PGP private", PGP_PRIVATE, "EXAMPLE_PGP_PRIVATE_BODY");
}

#[test]
fn fixture_pgp_public_block() {
    assert_fully_scrubbed("PGP public", PGP_PUBLIC, "EXAMPLE_PGP_PUBLIC_BODY");
}

#[test]
fn fixture_pgp_encrypted_message() {
    assert_fully_scrubbed("PGP message", PGP_MESSAGE, "EXAMPLE_PGP_ENCRYPTED_BODY");
}

#[test]
fn fixture_pgp_signed_message_asymmetric_end_marker() {
    // BEGIN PGP SIGNED MESSAGE / END PGP SIGNATURE — note the
    // trailing END marker does NOT match the BEGIN marker. The
    // catalog handles the asymmetric pair via independent
    // alternation arms.
    assert_fully_scrubbed("PGP signed", PGP_SIGNED, "EXAMPLE_PGP_SIGNATURE_BODY");
}

// ---------------------------------------------------------------------------
// Real-world paste scenarios — multiple keys / mixed content
// ---------------------------------------------------------------------------

#[test]
fn fixture_two_ssh_keys_in_one_paste_redact_independently() {
    // Reluctant-quantifier proof: two adjacent PEM blocks must not
    // collapse into a single redaction span covering everything
    // between the first BEGIN and the last END.
    let r = Redactor::new();
    let envelope = format!("first key:\n{SSH_RSA}\n\nsecond key:\n{SSH_OPENSSH}\n");
    let out = r.redact(&envelope);
    let count = out.matches(REDACTED).count();
    assert_eq!(
        count, 2,
        "ft-2xkrc: expected 2 independent redactions; got {count} in {out:?}"
    );
    assert!(!out.contains("EXAMPLE_FIXTURE"));
    assert!(!out.contains("EXAMPLE_OPENSSH_BODY"));
}

#[test]
fn fixture_ssh_key_alongside_pgp_block_redact_independently() {
    let r = Redactor::new();
    let envelope = format!("ssh:\n{SSH_RSA}\n\ngpg:\n{PGP_PRIVATE}\n");
    let out = r.redact(&envelope);
    let count = out.matches(REDACTED).count();
    assert_eq!(
        count, 2,
        "ft-2xkrc: expected 1 ssh + 1 pgp redaction; got {count} in {out:?}"
    );
    assert!(!out.contains("EXAMPLE_FIXTURE"));
    assert!(!out.contains("EXAMPLE_PGP_PRIVATE_BODY"));
}

#[test]
fn fixture_pasted_cat_id_rsa_session_log_scrubs_key_only() {
    // Realistic terminal session: shell prompt + cat command +
    // key body + next prompt. Only the PEM block redacts; the
    // surrounding shell text passes through verbatim.
    let r = Redactor::new();
    let session = format!(
        "user@host:~$ cat ~/.ssh/id_rsa\n\
         {SSH_RSA}\n\
         user@host:~$ echo done\n\
         done\n"
    );
    let out = r.redact(&session);
    assert!(out.contains("user@host:~$ cat ~/.ssh/id_rsa"));
    assert!(out.contains("user@host:~$ echo done"));
    assert!(out.contains("done"));
    assert!(out.contains(REDACTED));
    assert!(!out.contains("EXAMPLE_FIXTURE"));
    assert!(!out.contains("MIIEpAIBAAKCAQEA"));
}

#[test]
fn fixture_pgp_signed_review_snippet_scrubs_signature_only() {
    // Realistic code-review scenario: a developer pastes a
    // PGP-signed message proving authorship. The signed PLAINTEXT
    // is part of the block — the regex spans the whole armoured
    // shape including the signature. Trade-off: redacting the
    // plaintext is acceptable here because the operator-pane
    // logging context cannot tell signed-data plaintext apart
    // from key material; defense-in-depth.
    let r = Redactor::new();
    let review = format!("review note:\nplease verify:\n{PGP_SIGNED}\nthanks!\n");
    let out = r.redact(&review);
    assert!(out.contains("review note:"));
    assert!(out.contains("thanks!"));
    assert!(out.contains(REDACTED));
    assert!(!out.contains("EXAMPLE_PGP_SIGNATURE_BODY"));
}

// ---------------------------------------------------------------------------
// Threshold tightening regressions (br-ft-2xkrc supplementary)
// ---------------------------------------------------------------------------

#[test]
fn fixture_anthropic_key_at_40_char_threshold_still_redacts() {
    // br-ft-2xkrc: ANTHROPIC_KEY body threshold tightened 20 → 40.
    // Real Anthropic keys are ~95 chars; a 40-char floor keeps
    // false-positive risk low while catching every real key.
    let r = Redactor::new();
    let key = "sk-ant-api03-aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890aBcDeFgHiJk";
    let out = r.redact(&format!("ANTHROPIC_API_KEY={key}"));
    assert!(!out.contains(key), "anthropic key leaked: {out:?}");
    assert!(out.contains(REDACTED));
}

#[test]
fn fixture_github_pat_at_50_char_threshold_still_redacts() {
    // br-ft-2xkrc: GITHUB_FINE_GRAINED_PAT body threshold
    // tightened 40 → 50. Real PATs are 93 chars total ≈ 82 body
    // chars; a 50-char floor still catches every real PAT.
    let r = Redactor::new();
    let pat = "github_pat_11ABCDEFG0aBcDeFg_HiJkLmNoPqRsTuVwXyZ1234567890ABCDE";
    let out = r.redact(&format!("GITHUB_TOKEN={pat}"));
    assert!(!out.contains(pat), "github PAT leaked: {out:?}");
    assert!(out.contains(REDACTED));
}

#[test]
fn fixture_aws_access_key_id_redacts_at_exact_format() {
    // br-ft-2xkrc: AWS_ACCESS_KEY_ID is `AKIA[0-9A-Z]{16}` —
    // exact format, no threshold variation. Test pins the format
    // contract.
    let r = Redactor::new();
    let key = "AKIAIOSFODNN7EXAMPLE";
    let out = r.redact(&format!("AWS_ACCESS_KEY_ID={key}"));
    assert!(!out.contains(key), "AWS access key leaked: {out:?}");
    assert!(out.contains(REDACTED));
}
