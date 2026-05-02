//! iTerm2 OSC 1337 protocol
//! ([BR-TERM-EMULATOR-UPLIFT-2.1.3] / `ft-2okh0.1.3`).
//!
//! iTerm2's OSC 1337 protocol is widely supported by Mac-
//! centric tools. Key commands: `File` (display image
//! inline), `SetColors` (palette change), `SetProfile`
//! (theme switch), `MultipartFile` (chunked upload).
//!
//! ## What this module ships
//!
//! - [`Osc1337Sub`] — closed taxonomy of 4 subcommands.
//! - [`parse_osc_1337`] — pure parser for the OSC body
//!   (`Sub=key=value;key=value:payload` form).
//! - [`MultipartFileBuffer`] — chunked-upload reassembly
//!   with out-of-order, missing-chunk, duplicate-chunk
//!   handling.
//! - [`SetColorsRequest`] — palette index/RGB pairs with
//!   bounds validation (0..=255).
//! - [`ProfileSwitchPolicy`] — security gate enum
//!   (`Allow` / `Prompt` / `Deny`) per the bead's
//!   `osc1337_profile_switch` config.
//! - [`evaluate_security_gate`] — pure decision tree.
//! - [`Osc1337Health`] — `ft doctor` snapshot.
//! - [`StructuredLogRow`] — JSONL row contract for
//!   `tests/osc_1337/logs/<scenario>.jsonl`.
//!
//! ## What this module is NOT
//!
//! - Not the OSC framing layer. The escape-parser strips
//!   the `ESC ] 1337 ;` envelope and feeds the body here.
//! - Not the image renderer. `File` and `MultipartFile`
//!   carry payloads; the Kitty graphics atlas (sibling
//!   `ft-2okh0.1.2`) is the storage path. This module
//!   ships the parsed envelope + payload bytes only.
//! - Not the prompt/UI surface. The security gate emits a
//!   decision; the GUI integration shows the modal.
//! - Not the palette/theme applier. `SetColors` /
//!   `SetProfile` produce parsed records; `color_management`
//!   applies them (cross-link `ft-mpc9b.10.3`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// ============================================================================
// Subcommand taxonomy
// ============================================================================

/// Closed list of OSC 1337 subcommands. Adding a new one
/// extends this enum; the `every_subcommand_has_a_slug`
/// test pins coverage.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Osc1337Sub {
    /// `File=name=...;size=...;...:base64payload`
    File {
        name: Option<String>,
        size_bytes: Option<u64>,
        inline: bool,
        base64_payload_len: u32,
    },
    /// `SetColors=<index>=<rgb>;<index>=<rgb>;...`
    SetColors { entries: Vec<PaletteEntry> },
    /// `SetProfile=<profile_name>`
    SetProfile { profile: String },
    /// `MultipartFile=...` envelopes; multiple frames
    /// reassemble into a single payload.
    MultipartFile {
        upload_id: String,
        chunk_index: u32,
        chunk_count: u32,
        base64_payload_len: u32,
    },
}

impl Osc1337Sub {
    /// Stable slug for telemetry + structured logs.
    #[must_use]
    pub fn slug(&self) -> &'static str {
        match self {
            Self::File { .. } => "file",
            Self::SetColors { .. } => "set_colors",
            Self::SetProfile { .. } => "set_profile",
            Self::MultipartFile { .. } => "multipart_file",
        }
    }
}

/// One palette entry: index (0..=255) + 24-bit RGB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PaletteEntry {
    pub index: u8,
    pub rgb: u32,
}

// ============================================================================
// Parser
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Osc1337ParseError {
    /// Body did not start with a known subcommand.
    UnknownSubcommand,
    /// Required arg missing or unparseable.
    MalformedArgs,
    /// Body truncated before the expected delimiter.
    Truncated,
    /// Numeric value out of range (e.g., palette index > 255).
    OutOfRange,
}

/// Parse an OSC 1337 body (everything after `1337;`).
///
/// Forms accepted:
/// - `File=key=value;key=value:base64`
/// - `SetColors=<index>=<rgb>;<index>=<rgb>;...`
/// - `SetProfile=<name>`
/// - `MultipartFile=key=value;...:base64`
pub fn parse_osc_1337(body: &str) -> Result<Osc1337Sub, Osc1337ParseError> {
    let body = body.trim();
    if body.is_empty() {
        return Err(Osc1337ParseError::Truncated);
    }
    let (sub, rest) = body
        .split_once('=')
        .ok_or(Osc1337ParseError::MalformedArgs)?;

    match sub {
        "File" => parse_file(rest),
        "SetColors" => parse_set_colors(rest),
        "SetProfile" => parse_set_profile(rest),
        "MultipartFile" => parse_multipart_file(rest),
        _ => Err(Osc1337ParseError::UnknownSubcommand),
    }
}

fn parse_file(rest: &str) -> Result<Osc1337Sub, Osc1337ParseError> {
    // `File=key=value;key=value:base64payload`
    let (args_part, payload) = match rest.split_once(':') {
        Some((a, p)) => (a, p),
        None => (rest, ""),
    };
    let args = parse_kv_args(args_part)?;
    let name = args.get("name").cloned();
    let size_bytes = args
        .get("size")
        .map(|s| s.parse::<u64>())
        .transpose()
        .map_err(|_| Osc1337ParseError::MalformedArgs)?;
    let inline = args.get("inline").is_some_and(|v| v == "1");
    Ok(Osc1337Sub::File {
        name,
        size_bytes,
        inline,
        base64_payload_len: payload.len() as u32,
    })
}

fn parse_set_colors(rest: &str) -> Result<Osc1337Sub, Osc1337ParseError> {
    // `SetColors=<index>=<rgb>;<index>=<rgb>;...`
    let mut entries = Vec::new();
    for pair in rest.split(';').filter(|p| !p.is_empty()) {
        let (idx_str, rgb_str) = pair
            .split_once('=')
            .ok_or(Osc1337ParseError::MalformedArgs)?;
        let index: u32 = idx_str
            .trim()
            .parse()
            .map_err(|_| Osc1337ParseError::MalformedArgs)?;
        if index > 255 {
            return Err(Osc1337ParseError::OutOfRange);
        }
        let rgb_str = rgb_str.trim().trim_start_matches('#');
        let rgb = u32::from_str_radix(rgb_str, 16).map_err(|_| Osc1337ParseError::MalformedArgs)?;
        if rgb > 0x00FF_FFFF {
            return Err(Osc1337ParseError::OutOfRange);
        }
        entries.push(PaletteEntry {
            index: index as u8,
            rgb,
        });
    }
    Ok(Osc1337Sub::SetColors { entries })
}

fn parse_set_profile(rest: &str) -> Result<Osc1337Sub, Osc1337ParseError> {
    let profile = rest.trim().to_string();
    if profile.is_empty() {
        return Err(Osc1337ParseError::MalformedArgs);
    }
    Ok(Osc1337Sub::SetProfile { profile })
}

fn parse_multipart_file(rest: &str) -> Result<Osc1337Sub, Osc1337ParseError> {
    let (args_part, payload) = match rest.split_once(':') {
        Some((a, p)) => (a, p),
        None => (rest, ""),
    };
    let args = parse_kv_args(args_part)?;
    let upload_id = args
        .get("id")
        .cloned()
        .ok_or(Osc1337ParseError::MalformedArgs)?;
    let chunk_index = args
        .get("chunk")
        .ok_or(Osc1337ParseError::MalformedArgs)?
        .parse::<u32>()
        .map_err(|_| Osc1337ParseError::MalformedArgs)?;
    let chunk_count = args
        .get("of")
        .ok_or(Osc1337ParseError::MalformedArgs)?
        .parse::<u32>()
        .map_err(|_| Osc1337ParseError::MalformedArgs)?;
    if chunk_count == 0 || chunk_index >= chunk_count {
        return Err(Osc1337ParseError::OutOfRange);
    }
    Ok(Osc1337Sub::MultipartFile {
        upload_id,
        chunk_index,
        chunk_count,
        base64_payload_len: payload.len() as u32,
    })
}

fn parse_kv_args(args: &str) -> Result<BTreeMap<String, String>, Osc1337ParseError> {
    let mut out = BTreeMap::new();
    for pair in args.split(';').filter(|p| !p.is_empty()) {
        let (k, v) = pair
            .split_once('=')
            .ok_or(Osc1337ParseError::MalformedArgs)?;
        out.insert(k.trim().to_string(), v.trim().to_string());
    }
    Ok(out)
}

// ============================================================================
// MultipartFile reassembly
// ============================================================================

/// Buffer for a single in-flight multipart upload. Holds
/// chunks keyed by index; final reassembly returns the
/// concatenated payload once every index is present.
///
/// **Integrity invariant**: fields are private. All
/// mutation goes through [`Self::feed`] (which enforces
/// the duplicate-check) or [`Self::finalize`] (which
/// consumes self). Direct field access would let a
/// maintainer insert bytes after the duplicate-check
/// fired, replace `chunk_count` to cause premature
/// completion, or zero `duplicate_count` to silence
/// telemetry — none of which is possible via the
/// accessor surface.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MultipartFileBuffer {
    upload_id: String,
    chunk_count: u32,
    chunks: BTreeMap<u32, Vec<u8>>,
    duplicate_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MultipartFeedOutcome {
    /// Chunk accepted; not yet complete.
    Accepted,
    /// Duplicate chunk — payload already present.
    Duplicate,
    /// Reassembly complete; consumer can take payload.
    Complete,
    /// Chunk index out of bounds for the upload.
    Rejected,
}

impl MultipartFileBuffer {
    /// Construct a new buffer for an in-flight upload.
    ///
    /// **Requires `chunk_count > 0`** to stay consistent
    /// with `parse_multipart_file`, which rejects
    /// `of=0` as `OutOfRange`. Calling with `chunk_count
    /// == 0` is a programming error: the buffer would
    /// report `is_complete()` immediately and `finalize()`
    /// would yield `Some(vec![])` — silently producing an
    /// "empty complete" upload that the parser layer
    /// would have rejected. Returns `None` rather than
    /// panicking so callers can surface the error.
    #[must_use]
    pub fn new(upload_id: String, chunk_count: u32) -> Option<Self> {
        if chunk_count == 0 {
            return None;
        }
        Some(Self {
            upload_id,
            chunk_count,
            chunks: BTreeMap::new(),
            duplicate_count: 0,
        })
    }

    /// Backwards-compatible constructor that panics on
    /// `chunk_count == 0`. Use [`Self::new`] for the
    /// fallible variant. Retained for tests + integration
    /// sites that statically know `chunk_count > 0`.
    #[must_use]
    pub fn new_or_panic(upload_id: String, chunk_count: u32) -> Self {
        Self::new(upload_id, chunk_count)
            .expect("MultipartFileBuffer::new_or_panic requires chunk_count > 0")
    }

    /// Read-only accessor for the upload id.
    #[must_use]
    pub fn upload_id(&self) -> &str {
        &self.upload_id
    }

    /// Read-only accessor for the expected chunk count.
    #[must_use]
    pub fn chunk_count(&self) -> u32 {
        self.chunk_count
    }

    /// Read-only accessor for the duplicate counter.
    #[must_use]
    pub fn duplicate_count(&self) -> u64 {
        self.duplicate_count
    }

    /// How many chunks have been received so far.
    #[must_use]
    pub fn received_chunks(&self) -> u32 {
        self.chunks.len() as u32
    }

    /// Feed one chunk's payload bytes. Out-of-order is
    /// fine — chunks reassemble by index, not arrival
    /// order.
    pub fn feed(&mut self, chunk_index: u32, payload: Vec<u8>) -> MultipartFeedOutcome {
        if chunk_index >= self.chunk_count {
            return MultipartFeedOutcome::Rejected;
        }
        if self.chunks.contains_key(&chunk_index) {
            self.duplicate_count = self.duplicate_count.saturating_add(1);
            return MultipartFeedOutcome::Duplicate;
        }
        self.chunks.insert(chunk_index, payload);
        if self.chunks.len() as u32 == self.chunk_count {
            MultipartFeedOutcome::Complete
        } else {
            MultipartFeedOutcome::Accepted
        }
    }

    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.chunks.len() as u32 == self.chunk_count
    }

    /// Drain into the concatenated payload. Returns `None`
    /// if any chunk is missing.
    pub fn finalize(self) -> Option<Vec<u8>> {
        if !self.is_complete() {
            return None;
        }
        let mut out = Vec::new();
        for idx in 0..self.chunk_count {
            // `is_complete()` guarantees every index is
            // populated, but we still guard against the
            // map walking an unexpected key.
            let Some(chunk) = self.chunks.get(&idx) else {
                return None;
            };
            out.extend_from_slice(chunk);
        }
        Some(out)
    }
}

// ============================================================================
// Security gate
// ============================================================================

/// Per-config policy for `SetProfile` / `SetColors`. Bead
/// names `osc1337_profile_switch` with values `allow` /
/// `prompt` / `deny`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileSwitchPolicy {
    Allow,
    Prompt,
    Deny,
}

impl Default for ProfileSwitchPolicy {
    fn default() -> Self {
        // Bead: "Default: osc1337_profile_switch = prompt".
        Self::Prompt
    }
}

/// Per-config policy for `File` subcommand with `inline=0`
/// (iTerm2's download-to-disk mode). The bead's privacy
/// assumption ("image bytes never on disk") only holds
/// for `inline=true` — `inline=0` writes arbitrary bytes
/// to the user's filesystem under operator-controlled
/// `name`. We default to `Deny` (strictest) because this
/// is rarely-needed and high-impact if abused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageDownloadPolicy {
    Allow,
    Prompt,
    Deny,
}

impl Default for ImageDownloadPolicy {
    fn default() -> Self {
        Self::Deny
    }
}

/// Combined policy bundle the integration passes into the
/// security gate. Adding a new subcommand-level policy
/// extends this struct + the gate's match.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Osc1337PolicyBundle {
    pub profile_switch: ProfileSwitchPolicy,
    pub image_download: ImageDownloadPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecurityGateDecision {
    /// No gate needed (e.g., `File` subcommand) or policy
    /// is `Allow`.
    Allow,
    /// Surface the modal prompt to the user.
    Prompt,
    /// Reject silently; emit a `denied` log row.
    Deny,
}

/// Decide what to do with a parsed subcommand under a
/// given policy bundle. The integration consumes the
/// decision: `Allow` runs the op, `Prompt` shows a modal,
/// `Deny` emits a denial log entry.
#[must_use]
pub fn evaluate_security_gate(
    sub: &Osc1337Sub,
    policy: Osc1337PolicyBundle,
) -> SecurityGateDecision {
    match sub {
        Osc1337Sub::File { inline, .. } => {
            if *inline {
                // Inline image display — bytes stay in
                // memory per the bead's "image bytes
                // never on disk" privacy rule.
                SecurityGateDecision::Allow
            } else {
                // inline=0 means iTerm2 download-to-disk —
                // gate via ImageDownloadPolicy.
                match policy.image_download {
                    ImageDownloadPolicy::Allow => SecurityGateDecision::Allow,
                    ImageDownloadPolicy::Prompt => SecurityGateDecision::Prompt,
                    ImageDownloadPolicy::Deny => SecurityGateDecision::Deny,
                }
            }
        }
        Osc1337Sub::MultipartFile { .. } => {
            // Multipart envelope — the assembled payload
            // dispatches into the appropriate downstream
            // gate (display vs disk). Pass-through here.
            SecurityGateDecision::Allow
        }
        Osc1337Sub::SetColors { .. } => {
            // Palette mutation is reverted on theme reload
            // (bead's stated mitigation). The high-contrast
            // override happens upstream in
            // `accessibility_preferences`. At this layer we
            // allow.
            SecurityGateDecision::Allow
        }
        Osc1337Sub::SetProfile { .. } => match policy.profile_switch {
            ProfileSwitchPolicy::Allow => SecurityGateDecision::Allow,
            ProfileSwitchPolicy::Prompt => SecurityGateDecision::Prompt,
            ProfileSwitchPolicy::Deny => SecurityGateDecision::Deny,
        },
    }
}

/// Backwards-compatible shim. Existing callers that only
/// know about profile-switch policy can use this; they get
/// the default `image_download = Deny` policy
/// automatically (strictest). New callers should use the
/// bundle-aware [`evaluate_security_gate`] directly.
#[must_use]
pub fn evaluate_security_gate_profile_only(
    sub: &Osc1337Sub,
    profile_switch_policy: ProfileSwitchPolicy,
) -> SecurityGateDecision {
    evaluate_security_gate(
        sub,
        Osc1337PolicyBundle {
            profile_switch: profile_switch_policy,
            image_download: ImageDownloadPolicy::default(),
        },
    )
}

// ============================================================================
// Health snapshot
// ============================================================================

/// `ft doctor` snapshot. Mirrors bead's
/// "osc1337_commands_total / by_subcommand /
/// rejected_count / profile_switch_*_count" indicators.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Osc1337Health {
    pub commands_total: u64,
    pub by_subcommand: BTreeMap<String, u64>,
    pub rejected_total: u64,
    pub profile_switch_prompts: u64,
    pub profile_switch_allows: u64,
    pub profile_switch_denies: u64,
    pub multipart_uploads_in_flight: u32,
    pub multipart_duplicates_total: u64,
}

impl Osc1337Health {
    #[must_use]
    pub fn baseline() -> Self {
        Self::default()
    }

    /// Fold one parsed-and-evaluated event into the
    /// snapshot.
    pub fn fold_event(&mut self, sub: &Osc1337Sub, decision: SecurityGateDecision) {
        self.commands_total = self.commands_total.saturating_add(1);
        *self
            .by_subcommand
            .entry(sub.slug().to_string())
            .or_insert(0) += 1;
        if matches!(sub, Osc1337Sub::SetProfile { .. }) {
            match decision {
                SecurityGateDecision::Allow => {
                    self.profile_switch_allows = self.profile_switch_allows.saturating_add(1);
                }
                SecurityGateDecision::Prompt => {
                    self.profile_switch_prompts = self.profile_switch_prompts.saturating_add(1);
                }
                SecurityGateDecision::Deny => {
                    self.profile_switch_denies = self.profile_switch_denies.saturating_add(1);
                }
            }
        }
    }

    /// Fold one rejected parse.
    pub fn fold_rejection(&mut self) {
        self.rejected_total = self.rejected_total.saturating_add(1);
    }

    /// True iff rejection rate is healthy (≤5%) and
    /// no obvious anomalies.
    ///
    /// Per ft-05poz fix (5th rubber-stamp pattern this review):
    /// when commands_total == 0, return true iff rejected_total
    /// is also 0. The previous short-circuit returned true even
    /// when every command had been rejected (100% rejection
    /// rate — the worst possible state).
    #[must_use]
    pub fn is_safe(&self) -> bool {
        if self.commands_total == 0 {
            // Truly idle is healthy. Pure rejections (no
            // accepted commands) is the pathological 100% case
            // and must NOT report healthy.
            return self.rejected_total == 0;
        }
        let total = self.commands_total + self.rejected_total;
        let rejection_ratio = self.rejected_total as f64 / total as f64;
        rejection_ratio <= 0.05
    }
}

// ============================================================================
// Structured log row
// ============================================================================

/// One JSONL row in `tests/osc_1337/logs/<scenario>.jsonl`.
///
/// Per-OSC: `ts_ns, subcommand, args_hash, accepted,
/// security_gate_decision, bytes`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StructuredLogRow {
    pub ts_ns: u64,
    pub subcommand: String,
    pub args_hash: u64,
    pub accepted: bool,
    pub security_gate_decision: String,
    pub bytes: u32,
}

#[must_use]
pub fn render_log_jsonl(rows: &[StructuredLogRow]) -> String {
    let mut out = String::new();
    for r in rows {
        let line = serde_json::to_string(r).expect("StructuredLogRow always serializes");
        out.push_str(&line);
        out.push('\n');
    }
    out
}

pub fn parse_log_jsonl(jsonl: &str) -> Result<Vec<StructuredLogRow>, serde_json::Error> {
    let mut out = Vec::new();
    for line in jsonl.lines() {
        if line.trim().is_empty() {
            continue;
        }
        out.push(serde_json::from_str(line)?);
    }
    Ok(out)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------------
    // Subcommand taxonomy
    // ------------------------------------------------------------------------

    #[test]
    fn every_subcommand_has_a_slug() {
        let slugs = [
            Osc1337Sub::File {
                name: None,
                size_bytes: None,
                inline: false,
                base64_payload_len: 0,
            }
            .slug(),
            Osc1337Sub::SetColors { entries: vec![] }.slug(),
            Osc1337Sub::SetProfile {
                profile: "x".to_string(),
            }
            .slug(),
            Osc1337Sub::MultipartFile {
                upload_id: "u".to_string(),
                chunk_index: 0,
                chunk_count: 1,
                base64_payload_len: 0,
            }
            .slug(),
        ];
        let unique: std::collections::HashSet<_> = slugs.iter().collect();
        assert_eq!(unique.len(), 4);
    }

    // ------------------------------------------------------------------------
    // Parser
    // ------------------------------------------------------------------------

    #[test]
    fn parse_file_full_args() {
        let parsed = parse_osc_1337("File=name=test.png;size=1024;inline=1:abcdef").unwrap();
        match parsed {
            Osc1337Sub::File {
                name,
                size_bytes,
                inline,
                base64_payload_len,
            } => {
                assert_eq!(name, Some("test.png".to_string()));
                assert_eq!(size_bytes, Some(1024));
                assert!(inline);
                assert_eq!(base64_payload_len, 6);
            }
            _ => panic!("expected File"),
        }
    }

    #[test]
    fn parse_file_minimal_args() {
        let parsed = parse_osc_1337("File=:abc").unwrap();
        match parsed {
            Osc1337Sub::File {
                name,
                size_bytes,
                inline,
                base64_payload_len,
            } => {
                assert_eq!(name, None);
                assert_eq!(size_bytes, None);
                assert!(!inline);
                assert_eq!(base64_payload_len, 3);
            }
            _ => panic!("expected File"),
        }
    }

    #[test]
    fn parse_set_colors_two_entries() {
        let parsed = parse_osc_1337("SetColors=0=ff0000;15=00ff00").unwrap();
        match parsed {
            Osc1337Sub::SetColors { entries } => {
                assert_eq!(entries.len(), 2);
                assert_eq!(
                    entries[0],
                    PaletteEntry {
                        index: 0,
                        rgb: 0xFF_00_00
                    }
                );
                assert_eq!(
                    entries[1],
                    PaletteEntry {
                        index: 15,
                        rgb: 0x00_FF_00
                    }
                );
            }
            _ => panic!("expected SetColors"),
        }
    }

    #[test]
    fn parse_set_colors_index_out_of_range_rejected() {
        let err = parse_osc_1337("SetColors=256=ff0000").unwrap_err();
        assert_eq!(err, Osc1337ParseError::OutOfRange);
    }

    #[test]
    fn parse_set_colors_rgb_out_of_range_rejected() {
        let err = parse_osc_1337("SetColors=0=ffffffff").unwrap_err();
        assert_eq!(err, Osc1337ParseError::OutOfRange);
    }

    #[test]
    fn parse_set_profile() {
        let parsed = parse_osc_1337("SetProfile=Solarized").unwrap();
        assert_eq!(
            parsed,
            Osc1337Sub::SetProfile {
                profile: "Solarized".to_string()
            }
        );
    }

    #[test]
    fn parse_set_profile_empty_rejected() {
        let err = parse_osc_1337("SetProfile=").unwrap_err();
        assert_eq!(err, Osc1337ParseError::MalformedArgs);
    }

    #[test]
    fn parse_multipart_file() {
        let parsed = parse_osc_1337("MultipartFile=id=upload-1;chunk=0;of=3:abc").unwrap();
        assert_eq!(
            parsed,
            Osc1337Sub::MultipartFile {
                upload_id: "upload-1".to_string(),
                chunk_index: 0,
                chunk_count: 3,
                base64_payload_len: 3,
            }
        );
    }

    #[test]
    fn parse_multipart_chunk_out_of_bounds_rejected() {
        let err = parse_osc_1337("MultipartFile=id=u;chunk=5;of=3:abc").unwrap_err();
        assert_eq!(err, Osc1337ParseError::OutOfRange);
    }

    #[test]
    fn parse_multipart_zero_chunks_rejected() {
        let err = parse_osc_1337("MultipartFile=id=u;chunk=0;of=0:abc").unwrap_err();
        assert_eq!(err, Osc1337ParseError::OutOfRange);
    }

    #[test]
    fn parse_unknown_subcommand_rejected() {
        let err = parse_osc_1337("Unknown=foo").unwrap_err();
        assert_eq!(err, Osc1337ParseError::UnknownSubcommand);
    }

    #[test]
    fn parse_empty_body_rejected() {
        let err = parse_osc_1337("").unwrap_err();
        assert_eq!(err, Osc1337ParseError::Truncated);
    }

    #[test]
    fn parse_truncated_no_equals_rejected() {
        let err = parse_osc_1337("File").unwrap_err();
        assert_eq!(err, Osc1337ParseError::MalformedArgs);
    }

    // ------------------------------------------------------------------------
    // Multipart reassembly
    // ------------------------------------------------------------------------

    #[test]
    fn multipart_in_order_completes() {
        let mut buf = MultipartFileBuffer::new_or_panic("u".to_string(), 3);
        assert_eq!(buf.feed(0, vec![1]), MultipartFeedOutcome::Accepted);
        assert_eq!(buf.feed(1, vec![2]), MultipartFeedOutcome::Accepted);
        assert_eq!(buf.feed(2, vec![3]), MultipartFeedOutcome::Complete);
        assert_eq!(buf.finalize(), Some(vec![1, 2, 3]));
    }

    #[test]
    fn multipart_out_of_order_completes() {
        let mut buf = MultipartFileBuffer::new_or_panic("u".to_string(), 3);
        assert_eq!(buf.feed(2, vec![3]), MultipartFeedOutcome::Accepted);
        assert_eq!(buf.feed(0, vec![1]), MultipartFeedOutcome::Accepted);
        assert_eq!(buf.feed(1, vec![2]), MultipartFeedOutcome::Complete);
        assert_eq!(buf.finalize(), Some(vec![1, 2, 3]));
    }

    #[test]
    fn multipart_duplicate_counted_not_overwritten() {
        let mut buf = MultipartFileBuffer::new_or_panic("u".to_string(), 2);
        assert_eq!(buf.feed(0, vec![1]), MultipartFeedOutcome::Accepted);
        assert_eq!(buf.feed(0, vec![99]), MultipartFeedOutcome::Duplicate);
        assert_eq!(buf.duplicate_count(), 1);
        assert_eq!(buf.feed(1, vec![2]), MultipartFeedOutcome::Complete);
        assert_eq!(buf.finalize(), Some(vec![1, 2]));
    }

    #[test]
    fn multipart_missing_chunk_does_not_complete() {
        let mut buf = MultipartFileBuffer::new_or_panic("u".to_string(), 3);
        assert_eq!(buf.feed(0, vec![1]), MultipartFeedOutcome::Accepted);
        assert_eq!(buf.feed(2, vec![3]), MultipartFeedOutcome::Accepted);
        assert!(!buf.is_complete());
        assert_eq!(buf.finalize(), None);
    }

    #[test]
    fn multipart_out_of_bounds_chunk_rejected() {
        let mut buf = MultipartFileBuffer::new_or_panic("u".to_string(), 3);
        assert_eq!(buf.feed(5, vec![1]), MultipartFeedOutcome::Rejected);
        assert!(!buf.is_complete());
    }

    #[test]
    fn multipart_fields_private_no_direct_mutation() {
        // Integrity invariant: cannot bypass duplicate
        // check via direct chunks insertion or zero
        // duplicate_count to silence telemetry. Field
        // privacy structurally enforces this — `buf.chunks =
        // ...`, `buf.duplicate_count = 0`, etc. would be
        // compile errors.
        let mut buf = MultipartFileBuffer::new_or_panic("upload-1".to_string(), 2);
        buf.feed(0, vec![1]);
        buf.feed(0, vec![99]); // duplicate
        assert_eq!(buf.duplicate_count(), 1);
        assert_eq!(buf.upload_id(), "upload-1");
        assert_eq!(buf.chunk_count(), 2);
        assert_eq!(buf.received_chunks(), 1);
    }

    #[test]
    fn multipart_new_rejects_zero_chunk_count() {
        // Consistency with parse_multipart_file: parser
        // rejects of=0 (OutOfRange). Constructor must
        // reject the same shape so direct callers can't
        // bypass and get an "empty complete" buffer.
        assert!(MultipartFileBuffer::new("u".to_string(), 0).is_none());
        assert!(MultipartFileBuffer::new("u".to_string(), 1).is_some());
    }

    #[test]
    #[should_panic(expected = "chunk_count > 0")]
    fn multipart_new_or_panic_with_zero_panics() {
        let _ = MultipartFileBuffer::new_or_panic("u".to_string(), 0);
    }

    #[test]
    fn multipart_empty_payload_chunk_accepted() {
        // Empty payload (b"") in a chunk is legal — some
        // multipart uploads can have zero-byte chunks at
        // the end (sentinel) or be entirely empty.
        let mut buf = MultipartFileBuffer::new_or_panic("u".to_string(), 2);
        assert_eq!(buf.feed(0, vec![]), MultipartFeedOutcome::Accepted);
        assert_eq!(buf.feed(1, vec![1, 2]), MultipartFeedOutcome::Complete);
        assert_eq!(buf.finalize(), Some(vec![1, 2]));
    }

    #[test]
    fn multipart_empty_upload_id_accepted_at_construction() {
        // Foundation slice: upload_id is just a
        // correlation key. Empty string is legal at this
        // layer — the parser layer enforces non-empty if
        // needed.
        let buf = MultipartFileBuffer::new_or_panic(String::new(), 1);
        assert_eq!(buf.upload_id(), "");
    }

    // ------------------------------------------------------------------------
    // Security gate
    // ------------------------------------------------------------------------

    #[test]
    fn file_inline_true_always_allowed() {
        let sub = Osc1337Sub::File {
            name: None,
            size_bytes: None,
            inline: true,
            base64_payload_len: 0,
        };
        for policy in [
            ProfileSwitchPolicy::Allow,
            ProfileSwitchPolicy::Prompt,
            ProfileSwitchPolicy::Deny,
        ] {
            assert_eq!(
                evaluate_security_gate_profile_only(&sub, policy),
                SecurityGateDecision::Allow,
                "inline-image display should always be Allowed"
            );
        }
    }

    #[test]
    fn file_inline_false_default_policy_denies() {
        // SECURITY REGRESSION TEST: previously, File with
        // inline=0 (download-to-disk) was Allowed
        // unconditionally, letting a malicious app write
        // arbitrary files to the user's filesystem under
        // operator-controlled `name`. The fix routes through
        // ImageDownloadPolicy (default Deny).
        let sub = Osc1337Sub::File {
            name: Some("evil.sh".to_string()),
            size_bytes: Some(100),
            inline: false,
            base64_payload_len: 50,
        };
        let policy = Osc1337PolicyBundle::default();
        assert_eq!(
            evaluate_security_gate(&sub, policy),
            SecurityGateDecision::Deny,
            "inline=0 (download-to-disk) must be denied by default"
        );
    }

    #[test]
    fn file_inline_false_with_prompt_policy_prompts() {
        let sub = Osc1337Sub::File {
            name: Some("file.txt".to_string()),
            size_bytes: Some(100),
            inline: false,
            base64_payload_len: 50,
        };
        let policy = Osc1337PolicyBundle {
            profile_switch: ProfileSwitchPolicy::default(),
            image_download: ImageDownloadPolicy::Prompt,
        };
        assert_eq!(
            evaluate_security_gate(&sub, policy),
            SecurityGateDecision::Prompt
        );
    }

    #[test]
    fn file_inline_false_with_allow_policy_allows() {
        let sub = Osc1337Sub::File {
            name: Some("file.txt".to_string()),
            size_bytes: Some(100),
            inline: false,
            base64_payload_len: 50,
        };
        let policy = Osc1337PolicyBundle {
            profile_switch: ProfileSwitchPolicy::default(),
            image_download: ImageDownloadPolicy::Allow,
        };
        assert_eq!(
            evaluate_security_gate(&sub, policy),
            SecurityGateDecision::Allow
        );
    }

    #[test]
    fn default_image_download_policy_is_deny() {
        // Strictest default — explicit operator opt-in
        // required to enable iTerm2 download-to-disk.
        assert_eq!(ImageDownloadPolicy::default(), ImageDownloadPolicy::Deny);
    }

    #[test]
    fn set_colors_always_allowed_at_this_layer() {
        let sub = Osc1337Sub::SetColors { entries: vec![] };
        for policy in [
            ProfileSwitchPolicy::Allow,
            ProfileSwitchPolicy::Prompt,
            ProfileSwitchPolicy::Deny,
        ] {
            assert_eq!(
                evaluate_security_gate_profile_only(&sub, policy),
                SecurityGateDecision::Allow
            );
        }
    }

    #[test]
    fn set_profile_default_policy_prompts() {
        let sub = Osc1337Sub::SetProfile {
            profile: "Theme".to_string(),
        };
        assert_eq!(
            evaluate_security_gate_profile_only(&sub, ProfileSwitchPolicy::default()),
            SecurityGateDecision::Prompt
        );
    }

    #[test]
    fn set_profile_allow_policy_skips_prompt() {
        let sub = Osc1337Sub::SetProfile {
            profile: "Theme".to_string(),
        };
        assert_eq!(
            evaluate_security_gate_profile_only(&sub, ProfileSwitchPolicy::Allow),
            SecurityGateDecision::Allow
        );
    }

    #[test]
    fn set_profile_deny_policy_denies() {
        let sub = Osc1337Sub::SetProfile {
            profile: "Theme".to_string(),
        };
        assert_eq!(
            evaluate_security_gate_profile_only(&sub, ProfileSwitchPolicy::Deny),
            SecurityGateDecision::Deny
        );
    }

    #[test]
    fn default_profile_switch_policy_is_prompt() {
        assert_eq!(ProfileSwitchPolicy::default(), ProfileSwitchPolicy::Prompt);
    }

    // ------------------------------------------------------------------------
    // Health snapshot
    // ------------------------------------------------------------------------

    #[test]
    fn health_baseline_safe() {
        assert!(Osc1337Health::baseline().is_safe());
    }

    #[test]
    fn health_fold_increments_per_subcommand() {
        let mut h = Osc1337Health::baseline();
        let file = Osc1337Sub::File {
            name: None,
            size_bytes: None,
            inline: false,
            base64_payload_len: 0,
        };
        h.fold_event(&file, SecurityGateDecision::Allow);
        h.fold_event(&file, SecurityGateDecision::Allow);
        assert_eq!(h.commands_total, 2);
        assert_eq!(h.by_subcommand.get("file"), Some(&2));
    }

    #[test]
    fn health_profile_switch_decisions_split_into_three_counters() {
        let mut h = Osc1337Health::baseline();
        let sub = Osc1337Sub::SetProfile {
            profile: "x".to_string(),
        };
        h.fold_event(&sub, SecurityGateDecision::Allow);
        h.fold_event(&sub, SecurityGateDecision::Prompt);
        h.fold_event(&sub, SecurityGateDecision::Prompt);
        h.fold_event(&sub, SecurityGateDecision::Deny);
        assert_eq!(h.profile_switch_allows, 1);
        assert_eq!(h.profile_switch_prompts, 2);
        assert_eq!(h.profile_switch_denies, 1);
    }

    #[test]
    fn health_unsafe_when_rejection_rate_exceeds_5pct() {
        let mut h = Osc1337Health::baseline();
        let sub = Osc1337Sub::File {
            name: None,
            size_bytes: None,
            inline: false,
            base64_payload_len: 0,
        };
        // 9 success + 1 reject = 10% rejected → unsafe
        for _ in 0..9 {
            h.fold_event(&sub, SecurityGateDecision::Allow);
        }
        h.fold_rejection();
        assert!(!h.is_safe());
    }

    #[test]
    fn health_safe_below_5pct_rejection_rate() {
        let mut h = Osc1337Health::baseline();
        let sub = Osc1337Sub::File {
            name: None,
            size_bytes: None,
            inline: false,
            base64_payload_len: 0,
        };
        for _ in 0..99 {
            h.fold_event(&sub, SecurityGateDecision::Allow);
        }
        h.fold_rejection();
        assert!(h.is_safe());
    }

    // ------------------------------------------------------------------------
    // Structured logging
    // ------------------------------------------------------------------------

    #[test]
    fn structured_log_jsonl_roundtrip() {
        let rows = vec![
            StructuredLogRow {
                ts_ns: 1_000_000,
                subcommand: "file".to_string(),
                args_hash: 0xDEAD_BEEF,
                accepted: true,
                security_gate_decision: "allow".to_string(),
                bytes: 1024,
            },
            StructuredLogRow {
                ts_ns: 2_000_000,
                subcommand: "set_profile".to_string(),
                args_hash: 0xBEEF_CAFE,
                accepted: false,
                security_gate_decision: "deny".to_string(),
                bytes: 32,
            },
        ];
        let jsonl = render_log_jsonl(&rows);
        let parsed = parse_log_jsonl(&jsonl).unwrap();
        assert_eq!(parsed, rows);
    }

    // ------------------------------------------------------------------------
    // Headline scenarios
    // ------------------------------------------------------------------------

    #[test]
    fn imgcat_scenario() {
        // Bead's stated user value: "imgcat (iTerm2 utility)
        // works in ft."
        let body = "File=name=cat.png;size=2048;inline=1:base64payloadhere";
        let parsed = parse_osc_1337(body).unwrap();
        let decision = evaluate_security_gate_profile_only(&parsed, ProfileSwitchPolicy::default());
        assert_eq!(decision, SecurityGateDecision::Allow);
        match parsed {
            Osc1337Sub::File {
                name,
                size_bytes,
                inline,
                base64_payload_len,
            } => {
                assert_eq!(name, Some("cat.png".to_string()));
                assert_eq!(size_bytes, Some(2048));
                assert!(inline);
                assert_eq!(base64_payload_len, "base64payloadhere".len() as u32);
            }
            _ => panic!("expected File"),
        }
    }

    #[test]
    fn theme_switch_default_prompts() {
        // Bead's stated security: "SetProfile: requires
        // user confirmation prompt unless config flag
        // osc1337_profile_switch = allow set."
        let body = "SetProfile=Solarized-Dark";
        let parsed = parse_osc_1337(body).unwrap();
        let decision = evaluate_security_gate_profile_only(&parsed, ProfileSwitchPolicy::default());
        assert_eq!(decision, SecurityGateDecision::Prompt);
    }

    #[test]
    fn multipart_three_chunk_reassembly() {
        // Bead's stated user value: "MultipartFile chunked
        // upload reassembly correctness (out-of-order,
        // missing, duplicate)."
        let mut buf = MultipartFileBuffer::new_or_panic("upload-42".to_string(), 3);
        buf.feed(2, vec![3, 4]);
        buf.feed(0, vec![1]);
        buf.feed(0, vec![99]); // duplicate
        let outcome = buf.feed(1, vec![2]);
        assert_eq!(outcome, MultipartFeedOutcome::Complete);
        assert_eq!(buf.duplicate_count(), 1);
        let finalized = buf.finalize().unwrap();
        assert_eq!(finalized, vec![1, 2, 3, 4]);
    }

    /// ft-05poz regression guard: previously is_safe returned
    /// true when commands_total == 0 regardless of rejected_total.
    /// That mis-classified the worst case (100% rejection rate)
    /// as healthy.
    #[test]
    fn health_is_safe_rejects_pure_rejection_storm() {
        let mut h = Osc1337Health::default();
        h.rejected_total = 10; // every command rejected
        h.commands_total = 0;
        assert!(!h.is_safe(), "100% rejection rate must NOT report healthy");
    }

    #[test]
    fn health_is_safe_accepts_truly_idle() {
        // Both counters zero = no commands processed yet = healthy.
        // Pin the boundary so the fix doesn't over-correct.
        let h = Osc1337Health::default();
        assert!(h.is_safe());
    }
}
