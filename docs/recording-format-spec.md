# WAR Recording Format Specification

**Bead:** wa-z0e.1
**Author:** TopazCliff
**Date:** 2026-02-04
**Status:** Draft

## Overview

This document specifies the `.war` (wa recording) file format for storing terminal session
recordings with efficient delta compression, rich metadata, and deterministic replay.
The format is designed to be parseable **without** wa, forward-compatible via versioning,
and safe to store (redaction-aware for sensitive content).

## Design Goals

1. Space-efficient recording via delta encoding and optional compression.
2. Accurate timing reconstruction (relative timestamps in milliseconds).
3. Rich metadata for context (pane snapshot, environment, tags).
4. Fast seeking via index section (keyframes + event lookup).
5. Forward compatibility with strict versioning rules.
6. Safe handling of sensitive content (redaction-first input capture).

## Non-Goals

- Perfect reconstruction of cursor/alt-screen state (best-effort only).
- Storing raw secrets or tokenized URLs (must be redacted or omitted).

## Encoding Conventions

- Integer fields are **little-endian**.
- Strings are UTF-8 and encoded as `u32 length + bytes` unless specified otherwise.
- Timestamps are `u64` milliseconds relative to recording start.
- JSON payloads are UTF-8 and length-prefixed.

## File Layout

```
[Header: fixed 256 bytes]
[Metadata: JSON, length-prefixed]
[Frames: binary stream]
[Index: binary + JSON summary]
[Footer: checksum + offsets]
```

## Header (Fixed 256 Bytes)

The header provides offsets, versioning, and global metadata. It is fixed-size to enable
fast parsing and direct seeking to each section.

```rust
pub struct WarHeader {
    magic: [u8; 4],        // "WAR\x01"
    version: u16,          // Format version (starting at 1)
    header_len: u16,       // Always 256 for v1
    flags: u32,            // Bitset (compression, redaction present, etc.)

    created_at_ms: i64,    // Unix epoch ms when recording started
    duration_ms: u64,      // Total recording duration (filled at close)

    metadata_offset: u64,
    metadata_len: u32,

    frames_offset: u64,    // Start of frame stream
    frames_len: u64,       // Total size of frames section

    index_offset: u64,
    index_len: u64,

    footer_offset: u64,

    compression: u16,      // 0 = none, 1 = lz4, 2 = zstd (v1 recommends lz4)
    checksum: u16,         // 0 = none, 1 = crc32, 2 = blake3

    reserved: [u8; 180],   // Must be zero; future expansion
}
```

**Header invariants**:
- `magic` must match exactly.
- `metadata_offset` must be >= `header_len`.
- Offsets must be monotonic: metadata → frames → index → footer.
- Unknown flags MUST be ignored by readers for forward compatibility.

## Metadata (JSON)

Metadata is a JSON object stored immediately after the header and referenced by
`metadata_offset/metadata_len`.

```json
{
  "pane": {
    "pane_id": 3,
    "domain": "local",
    "title": "codex",
    "cwd": "/home/user/project",
    "rows": 40,
    "cols": 120
  },
  "agent": {
    "type": "codex",
    "version": "vX.Y",
    "session_id": "optional"
  },
  "tags": ["demo", "compaction"],
  "description": "Compaction workflow run",
  "redaction": {
    "input_captured": false,
    "redaction_rules": ["token_url", "api_key"]
  }
}
```

Metadata is **advisory** and may be incomplete. Readers must tolerate missing fields.

## Frame Stream

Frames encode time-ordered events for replay. Each frame has a compact header and
a variable payload.

```rust
pub struct FrameHeader {
    timestamp_ms: u64,  // Relative to recording start
    frame_type: u8,     // FrameType enum
    flags: u8,          // Type-specific flags
    payload_len: u32,   // Length of payload bytes
}
```

### Frame Types

```rust
pub enum FrameType {
    Output = 1,   // Terminal output delta
    Resize = 2,   // Terminal size change
    Event = 3,    // wa detection event
    Marker = 4,   // User annotation
    Input = 5,    // Optional captured input (redacted)
}
```

### Output Frame (Delta Encoding)

Output frames store terminal output deltas relative to previous frames.

```rust
pub enum DeltaEncoding {
    Full { data: Vec<u8> },
    Diff { base_frame: u32, ops: Vec<DiffOp> },
    Repeat { base_frame: u32 },
}

pub enum DiffOp {
    Copy { offset: u32, len: u32 },
    Insert { data: Vec<u8> },
}
```

**Delta rules**:
- `Full` is used for the first frame or after discontinuities.
- `Diff` uses copy/insert operations relative to `base_frame`.
- `Repeat` references a prior frame with identical output.

**Compression**:
- If header `compression != 0`, compress **payload bytes** (not the frame header).
- LZ4 is recommended for speed and streaming friendliness.

### Resize Frame

```rust
pub struct ResizeFrame {
    rows: u32,
    cols: u32,
}
```

### Event Frame

Event frames store wa detection events in redacted JSON form.

```json
{
  "rule_id": "codex.usage.warning_10",
  "event_type": "usage.warning",
  "severity": "warning",
  "extracted": {"remaining": "10%"}
}
```

### Marker Frame

Marker frames are user annotations (e.g., highlights during replay).

```json
{
  "label": "checkpoint",
  "note": "before sending /compact"
}
```

### Input Frame (Redacted)

Input capture is optional and **must** be redacted. Store only safe summaries.

```json
{
  "text_len": 42,
  "preview_redacted": "git status",
  "text_hash": "sha256:...",
  "command_candidate": true
}
```

If redaction is not possible, omit input frames entirely.

## Index Section

The index enables fast seeking and event lookup. It contains a binary index followed by
a small JSON summary for readability.

```rust
pub struct FrameIndex {
    keyframes: Vec<Keyframe>,   // Seek targets
    events: Vec<EventIndex>,    // Event timeline
}

pub struct Keyframe {
    timestamp_ms: u64,
    frame_index: u64,
    file_offset: u64,
}

pub struct EventIndex {
    timestamp_ms: u64,
    frame_index: u64,
    rule_id: String,
}
```

Suggested policy: insert a keyframe every N seconds or M frames.

## Footer

The footer records the checksum and offsets for integrity verification.

```rust
pub struct WarFooter {
    checksum: [u8; 32],     // For blake3, otherwise truncated
    checksum_type: u16,
    index_offset: u64,
    index_len: u64,
    frames_len: u64,
}
```

Readers should verify checksum if supported; otherwise continue with a warning.

## Versioning Rules

- `version` starts at 1.
- Readers must reject unknown **major** versions.
- Minor/patch changes must be backward-compatible and use reserved fields.

## Redaction Rules

- Never store raw secrets, tokenized URLs, or session cookies.
- Input frames must always store redacted previews or be omitted.
- Event and metadata JSON must run through the same redactor as wa output.

## Testing Requirements

- Encode/decode roundtrip tests for header/frames/index.
- Property tests for delta correctness (full + diff + repeat).
- Fuzz tests for malformed header/offsets and corrupted payloads.
- Checksum verification tests (accept invalid checksum with explicit error).

## Open Questions

- Whether to adopt a container format (e.g., zstd dictionary) in v2.
- Whether to include optional raw scrollback snapshots as keyframes.
