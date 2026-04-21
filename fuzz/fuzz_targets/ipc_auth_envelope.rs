#![no_main]
//! IPC auth pipeline fuzz target.
//!
//! ## What this fuzzes
//!
//! The narrowest input-processing boundary on the ft IPC socket:
//!
//! ```text
//! raw UTF-8 line (≤ MAX_MESSAGE_SIZE bytes)
//!   → serde_json::from_str::<IpcEnvelope>
//!   → IpcAuth::authorize(envelope.token, request.required_scope())
//! ```
//!
//! Both halves matter:
//!
//! 1. **Envelope parsing.** `IpcEnvelope` uses `#[serde(flatten)]` on the
//!    `request: IpcRequest` field, which means serde will try every
//!    untagged variant against the incoming JSON. Malformed, truncated,
//!    oversized, or adversarially-shaped JSON must never panic, must
//!    never OOM, must never recurse to stack overflow.
//!
//! 2. **IpcAuth::authorize.** The sec/[HIGH] fix at 5810a3a4 rewrote
//!    this from a short-circuiting `.find()` to a constant-time scan
//!    over `Vec<IpcAuthToken>`. The invariant to pin under fuzzing:
//!    - No panic on any presented-token byte sequence (empty, multi-
//!      byte UTF-8, null bytes embedded — serde_json will have
//!      already rejected non-UTF-8, so tokens are always `&str`).
//!    - Determinism: same bytes + same auth → same status token.
//!    - Return value is one of the 7 documented status strings.
//!
//! ## Harness shape (Archetype 5: Structure-Aware + Archetype 1: Crash Detector)
//!
//! Uses `arbitrary::Arbitrary` to build a structured envelope document
//! (valid scaffold with mutation points) so we spend coverage on the
//! auth pipeline rather than on reaching the first JSON brace. The
//! mutator can drop required fields, mistype them, oversize them,
//! embed null bytes in the token, or swap the flattened request
//! variant tag — any of which could hit an un-tested branch.
//!
//! ## Oracle
//!
//! Dominant: **crash detector** (no panic, no stack overflow, no OOM).
//! Secondary: **determinism invariant** — the same input decoded twice
//! must produce the same status string.
//!
//! ## Not fuzzed here
//!
//! - The socket read loop itself. That's bounded by `MAX_MESSAGE_SIZE+1`
//!   at the transport layer and exercised by integration tests. Our
//!   job is the parse-and-authorize pipeline that it feeds.
//! - Expiry-clock skew. `IpcAuth::authorize` reads `now_ms()` from the
//!   system clock; the fuzz harness uses only `expires_at_ms = None` or
//!   far-future values to keep results deterministic under libFuzzer
//!   reruns. See [ft-0e179-adjacent follow-up] for a virtual-time seam
//!   if clock-skew fuzzing becomes valuable.

use arbitrary::Arbitrary;
use frankenterm_core::config::{IpcAuthToken, IpcScope};
use frankenterm_core::ipc::{
    __fuzz_constant_time_eq, __fuzz_parse_envelope_and_authorize, IpcAuth,
};
use libfuzzer_sys::fuzz_target;
use serde_json::{Map, Value};

// Guard rails — derived from ipc.rs MAX_MESSAGE_SIZE (131_072) and the
// realistic upper bound on how many tokens the ops team ever deploys.
const MAX_LINE_BYTES: usize = 131_072;
const MAX_TOKENS: usize = 16;
const MAX_TOKEN_STR_LEN: usize = 512;
const MAX_STR_LEN: usize = 256;
const MAX_SCOPES_PER_TOKEN: usize = 4;

#[derive(Arbitrary, Debug)]
struct FuzzInput {
    /// Auth configuration the authorize() path is evaluated against.
    /// Empty vec → authorize() unconditionally returns Ok — we still
    /// fuzz the parser in that mode for JSON-only coverage.
    tokens: FuzzAuthTokens,
    /// The envelope document to send through parse-and-authorize.
    envelope: FuzzEnvelope,
    /// If set, a second fuzz run through authorize() to pin determinism.
    check_determinism: bool,
    /// Independent probe into the constant-time compare helper —
    /// libFuzzer will minimise both branches toward interesting inputs.
    probe: ConstantTimeProbe,
}

#[derive(Arbitrary, Debug)]
struct FuzzAuthTokens {
    entries: Vec<FuzzToken>,
}

#[derive(Arbitrary, Debug)]
struct FuzzToken {
    token: String,
    scopes: Vec<FuzzScope>,
    // `Option<u64>` alone produces absurdly-far-future timestamps under
    // libFuzzer — we only need present/absent to exercise the branch.
    has_expiry: bool,
}

#[derive(Arbitrary, Debug, Clone, Copy)]
enum FuzzScope {
    Read,
    Write,
    All,
}

impl FuzzScope {
    fn into_ipc(self) -> IpcScope {
        match self {
            Self::Read => IpcScope::Read,
            Self::Write => IpcScope::Write,
            Self::All => IpcScope::All,
        }
    }
}

#[derive(Arbitrary, Debug)]
struct FuzzEnvelope {
    mode: EnvelopeMode,
    token_variant: TokenVariant,
    request_id: Option<String>,
    request_kind: RequestKind,
    /// Extra noise fields at the top level so the `#[serde(flatten)]`
    /// path encounters unexpected neighbours next to `kind` / `token`.
    extra_fields: Vec<ExtraField>,
}

#[derive(Arbitrary, Debug)]
enum EnvelopeMode {
    /// Well-formed JSON object.
    Valid,
    /// Drop a required field from the flattened request.
    DropKind,
    /// Emit the request's `kind` field with the wrong type.
    MistypedKind(WrongValue),
    /// Send a top-level non-object (array/string/null).
    TopLevelIs(TopLevelShape),
    /// Append garbage bytes AFTER a valid JSON object (tests line-
    /// reader's strictness about trailing content).
    TrailingGarbage(Vec<u8>),
    /// Truncate the JSON to a short prefix.
    TruncatePrefix(u16),
}

#[derive(Arbitrary, Debug)]
enum TokenVariant {
    /// No `token` field at all.
    Absent,
    /// `token: null`.
    Null,
    /// `token: "..."` with arbitrary bytes (always valid UTF-8 because
    /// Arbitrary<String> guarantees it).
    Present(String),
    /// `token: 42` — wrong type, tests the Option<String> deserializer.
    WrongType,
    /// `token: ""` — empty string (distinct from Absent in auth logic).
    Empty,
    /// Extremely long token to probe MAX_MESSAGE_SIZE interaction.
    Long(u16),
}

#[derive(Arbitrary, Debug)]
enum RequestKind {
    Ping,
    Status,
    PaneState { pane_id: u64 },
    UserVar { key: String, value: String },
    SetPanePriority { pane_id: u64, level: u8 },
    ClearPanePriority { pane_id: u64 },
    /// Unknown kind — tests the serde untagged fallthrough.
    Unknown(String),
}

#[derive(Arbitrary, Debug)]
enum WrongValue {
    Null,
    Bool,
    Number,
    Array,
    Object,
}

#[derive(Arbitrary, Debug)]
enum TopLevelShape {
    Null,
    Bool,
    Number,
    String(String),
    Array,
}

#[derive(Arbitrary, Debug)]
struct ExtraField {
    key: String,
    value: ExtraValue,
}

#[derive(Arbitrary, Debug)]
enum ExtraValue {
    Null,
    Bool(bool),
    Int(i64),
    Text(String),
}

#[derive(Arbitrary, Debug)]
struct ConstantTimeProbe {
    a: String,
    b: String,
}

impl FuzzInput {
    fn into_parts(self) -> (IpcAuth, Vec<u8>, bool, ConstantTimeProbe) {
        let auth = IpcAuth::new(self.tokens.into_ipc());
        let line = self.envelope.into_bytes();
        (auth, line, self.check_determinism, self.probe)
    }
}

impl FuzzAuthTokens {
    fn into_ipc(self) -> Vec<IpcAuthToken> {
        self.entries
            .into_iter()
            .take(MAX_TOKENS)
            .map(|t| IpcAuthToken {
                token: bound_string(t.token, MAX_TOKEN_STR_LEN),
                scopes: if t.scopes.is_empty() {
                    vec![IpcScope::All]
                } else {
                    t.scopes
                        .into_iter()
                        .take(MAX_SCOPES_PER_TOKEN)
                        .map(FuzzScope::into_ipc)
                        .collect()
                },
                // Only exercise "no expiry" and "in the far future" so
                // `now_ms()` doesn't introduce clock-driven non-
                // determinism across libFuzzer reruns.
                expires_at_ms: if t.has_expiry {
                    Some(u64::MAX / 2)
                } else {
                    None
                },
            })
            .collect()
    }
}

impl FuzzEnvelope {
    fn into_bytes(self) -> Vec<u8> {
        let mut obj = Map::new();

        // Start by building a well-formed request-kind structure.
        let (kind_key, kind_value, rest) = self.request_kind.into_json();
        obj.insert(kind_key.to_string(), kind_value);
        for (k, v) in rest {
            obj.insert(k, v);
        }

        // Token field.
        match self.token_variant {
            TokenVariant::Absent => {}
            TokenVariant::Null => {
                obj.insert("token".into(), Value::Null);
            }
            TokenVariant::Present(s) => {
                obj.insert("token".into(), Value::String(bound_string(s, MAX_STR_LEN)));
            }
            TokenVariant::WrongType => {
                obj.insert("token".into(), Value::from(42));
            }
            TokenVariant::Empty => {
                obj.insert("token".into(), Value::String(String::new()));
            }
            TokenVariant::Long(n) => {
                // Cap at MAX_TOKEN_STR_LEN so we don't spend all our
                // exec time serialising megabyte strings — the goal is
                // to hit the IpcAuth scan loop with a long comparand,
                // not to stress serde_json.
                let len = (usize::from(n) % MAX_TOKEN_STR_LEN).max(1);
                obj.insert("token".into(), Value::String("a".repeat(len)));
            }
        }

        // request_id.
        if let Some(rid) = self.request_id {
            obj.insert(
                "request_id".into(),
                Value::String(bound_string(rid, MAX_STR_LEN)),
            );
        }

        // Extra fields — a few more keys at the top level to make sure
        // the flatten boundary doesn't misinterpret them.
        for extra in self.extra_fields.into_iter().take(4) {
            let key = bound_string(extra.key, 32);
            if key.is_empty() || matches!(key.as_str(), "token" | "request_id" | "kind") {
                continue;
            }
            obj.insert(key, extra.value.into_json());
        }

        // Apply the mutation mode.
        match self.mode {
            EnvelopeMode::Valid => serde_json::to_vec(&Value::Object(obj)).unwrap_or_default(),
            EnvelopeMode::DropKind => {
                obj.remove("kind");
                serde_json::to_vec(&Value::Object(obj)).unwrap_or_default()
            }
            EnvelopeMode::MistypedKind(wrong) => {
                obj.insert("kind".into(), wrong.into_json());
                serde_json::to_vec(&Value::Object(obj)).unwrap_or_default()
            }
            EnvelopeMode::TopLevelIs(shape) => serde_json::to_vec(&shape.into_json())
                .unwrap_or_default(),
            EnvelopeMode::TrailingGarbage(garbage) => {
                let mut bytes = serde_json::to_vec(&Value::Object(obj)).unwrap_or_default();
                // Cap the garbage to ~1KB so the line stays under
                // MAX_LINE_BYTES without per-call allocation pressure.
                bytes.extend(garbage.into_iter().take(1024));
                bytes
            }
            EnvelopeMode::TruncatePrefix(n) => {
                let bytes = serde_json::to_vec(&Value::Object(obj)).unwrap_or_default();
                let take = (usize::from(n) % bytes.len().max(1)).min(bytes.len());
                bytes[..take].to_vec()
            }
        }
    }
}

impl RequestKind {
    fn into_json(self) -> (&'static str, Value, Vec<(String, Value)>) {
        match self {
            Self::Ping => ("kind", Value::String("ping".into()), vec![]),
            Self::Status => ("kind", Value::String("status".into()), vec![]),
            Self::PaneState { pane_id } => (
                "kind",
                Value::String("pane_state".into()),
                vec![("pane_id".into(), Value::from(pane_id))],
            ),
            Self::UserVar { key, value } => (
                "kind",
                Value::String("user_var".into()),
                vec![
                    ("key".into(), Value::String(bound_string(key, MAX_STR_LEN))),
                    ("value".into(), Value::String(bound_string(value, MAX_STR_LEN))),
                ],
            ),
            Self::SetPanePriority { pane_id, level } => (
                "kind",
                Value::String("set_pane_priority".into()),
                vec![
                    ("pane_id".into(), Value::from(pane_id)),
                    ("level".into(), Value::from(level)),
                ],
            ),
            Self::ClearPanePriority { pane_id } => (
                "kind",
                Value::String("clear_pane_priority".into()),
                vec![("pane_id".into(), Value::from(pane_id))],
            ),
            Self::Unknown(s) => (
                "kind",
                Value::String(bound_string(s, 32)),
                vec![],
            ),
        }
    }
}

impl WrongValue {
    fn into_json(self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Bool => Value::Bool(true),
            Self::Number => Value::from(1_000),
            Self::Array => Value::Array(vec![Value::from(1), Value::from(2)]),
            Self::Object => Value::Object(Map::new()),
        }
    }
}

impl TopLevelShape {
    fn into_json(self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Bool => Value::Bool(false),
            Self::Number => Value::from(0),
            Self::String(s) => Value::String(bound_string(s, MAX_STR_LEN)),
            Self::Array => Value::Array(vec![Value::Null]),
        }
    }
}

impl ExtraValue {
    fn into_json(self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Bool(b) => Value::Bool(b),
            Self::Int(i) => Value::from(i),
            Self::Text(s) => Value::String(bound_string(s, MAX_STR_LEN)),
        }
    }
}

fn bound_string(s: String, max: usize) -> String {
    s.chars().take(max).collect()
}

fn is_valid_status(status: &str) -> bool {
    matches!(
        status,
        "not_utf8" | "parse_err" | "allowed" | "missing" | "invalid" | "expired" | "scope"
    )
}

fuzz_target!(|input: FuzzInput| {
    let (auth, line, check_determinism, probe) = input.into_parts();

    // Hard guard — libFuzzer can synthesise very large inputs if we
    // don't clip. This mirrors the MAX_MESSAGE_SIZE+1 guard in the
    // real line-reader without re-implementing it.
    if line.len() > MAX_LINE_BYTES {
        return;
    }

    // ── Crash oracle #1: parse + authorize must never panic ──
    let status = __fuzz_parse_envelope_and_authorize(&line, &auth);

    // ── Invariant: the status MUST be one of the seven documented
    // tokens. Any other return value is a fuzz-seam contract break.
    assert!(
        is_valid_status(status),
        "ipc_auth fuzz seam returned undocumented status {status:?}"
    );

    // ── Determinism invariant: same bytes + same auth → same status.
    // A drift here means something inside parse-or-authorize observed
    // non-deterministic state (clock, RNG, HashMap ordering). Both
    // have been scrubbed in the fuzz setup (expiry is None or far-
    // future; IpcAuth is a Vec not a HashMap), so a failure here is
    // a real bug.
    if check_determinism {
        let second = __fuzz_parse_envelope_and_authorize(&line, &auth);
        assert_eq!(status, second, "authorize is non-deterministic on same input");
    }

    // ── Crash oracle #2: ipc_constant_time_eq must never panic on
    // any pair of strings, including length-zero and mismatched
    // lengths. Separately from the pipeline fuzz so libFuzzer can
    // minimise interesting inputs independently.
    let a = bound_string(probe.a, MAX_TOKEN_STR_LEN);
    let b = bound_string(probe.b, MAX_TOKEN_STR_LEN);
    let eq = __fuzz_constant_time_eq(&a, &b);
    // Symmetry invariant: eq(a, b) == eq(b, a).
    let eq_swapped = __fuzz_constant_time_eq(&b, &a);
    assert_eq!(eq, eq_swapped, "constant_time_eq is not symmetric");
    // Reflexivity: eq(a, a) must be true.
    assert!(
        __fuzz_constant_time_eq(&a, &a),
        "constant_time_eq fails reflexivity on {:?}",
        a
    );
});
