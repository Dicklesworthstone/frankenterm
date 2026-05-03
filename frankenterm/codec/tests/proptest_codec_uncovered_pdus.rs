//! Conformance roundtrip harness for PDU variants / branches that the main
//! `proptest_codec_roundtrip.rs` suite does not exercise.
//!
//! Isolated into its own test binary so these additions are independently
//! buildable and runnable even while the sibling file carries pre-existing
//! strategy-type drift (u64 vs usize field mismatches across ~60 legacy
//! strategies). That drift is pane 3's abandoned work and is out of scope
//! for this ship.
//!
//! Coverage added:
//!
//!   * `Pdu::MovePaneToNewTab` (the request variant) — the only entry in
//!     the `pdu!{}` registry at codec/src/lib.rs:596 that lacks a
//!     call site in the main roundtrip suite (only the `Response`
//!     variant was covered).
//!
//!   * `Pdu::SpawnV2 { command: Some(_) }` — the existing
//!     `spawn_v2_json_and_pdu_roundtrip` is fed by a strategy that pins
//!     `command: None`, so the `Some(CommandBuilder)` branch of the
//!     wire format was never actually hit by the roundtrip suite. A
//!     regression in `CommandBuilder`'s serde adapter (e.g. the
//!     env-map fix landed in ft-z5dxg / ft-rrrn5) could slip past.
//!
//! Each case now runs through all compression modes so these uncovered
//! branches also exercise the explicit wire framing modes, not only the
//! default auto-compression path.

use codec::{
    CompressionMode, ErrorResponse, MovePaneToNewTab, Pdu, Ping, Pong, Resize, SendPaste,
    SetClipboard, SetPaneZoomed, SetWindowWorkspace, SpawnResponse, SpawnV2, SplitPane,
    UnitResponse, WriteToPane,
};
use config::keyassignment::SpawnTabDomain;
use frankenterm_term::ClipboardSelection;
use frankenterm_term::TerminalSize;
use mux::tab::{SplitDirection, SplitRequest, SplitSize};
use portable_pty::CommandBuilder;
use proptest::prelude::*;
use std::convert::TryInto;

const ALL_COMPRESSION_MODES: [CompressionMode; 3] = [
    CompressionMode::Auto,
    CompressionMode::Never,
    CompressionMode::Always,
];
const COMPRESSED_MASK: u64 = 1 << 63;

fn arb_small_string() -> impl Strategy<Value = String> {
    proptest::collection::vec(any::<char>(), 0..16).prop_map(|chars| chars.into_iter().collect())
}

fn build_generated_command(
    argv0: &str,
    extra_args: &[String],
    env_pairs: &[(String, String)],
    cwd: Option<&String>,
) -> CommandBuilder {
    let mut cmd = CommandBuilder::new(argv0);
    cmd.env_clear();
    for arg in extra_args {
        cmd.arg(arg);
    }
    for (key, value) in env_pairs {
        cmd.env(key, value);
    }
    if let Some(dir) = cwd {
        cmd.cwd(dir);
    }
    cmd
}

fn assert_pdu_roundtrip(serial: u64, pdu: Pdu) {
    assert_pdu_roundtrip_with_mode(serial, &pdu, CompressionMode::Auto);
    assert_pdu_roundtrip_with_mode(serial, &pdu, CompressionMode::Never);
    assert_pdu_roundtrip_with_mode(serial, &pdu, CompressionMode::Always);
}

fn assert_pdu_roundtrip_with_mode(serial: u64, pdu: &Pdu, mode: CompressionMode) {
    let mut encoded = Vec::new();
    pdu.encode_with_mode(&mut encoded, serial, mode).unwrap();

    let decoded = Pdu::decode(encoded.as_slice()).unwrap();
    assert_eq!(decoded.serial, serial);
    assert_eq!(decoded.pdu, *pdu);

    let mut streaming = encoded.clone();
    let streamed = Pdu::stream_decode(&mut streaming).unwrap().unwrap();
    assert_eq!(streamed.serial, serial);
    assert_eq!(streamed.pdu, *pdu);
    assert!(streaming.is_empty());
}

#[derive(Clone, Debug)]
enum WireFramingPdu {
    Ping,
    Pong,
    UnitResponse,
    ErrorResponse(String),
    WriteToPane {
        pane_id: usize,
        data: Vec<u8>,
    },
    SendPaste {
        pane_id: usize,
        data: String,
    },
    SetClipboard {
        pane_id: usize,
        clipboard: Option<String>,
        selection: ClipboardSelection,
    },
    SetWindowWorkspace {
        window_id: usize,
        workspace: String,
    },
    Resize {
        containing_tab_id: usize,
        pane_id: usize,
        rows: usize,
        cols: usize,
    },
    SetPaneZoomed {
        containing_tab_id: usize,
        pane_id: usize,
        zoomed: bool,
    },
}

impl WireFramingPdu {
    fn to_pdu(&self) -> Pdu {
        match self {
            Self::Ping => Pdu::Ping(Ping {}),
            Self::Pong => Pdu::Pong(Pong {}),
            Self::UnitResponse => Pdu::UnitResponse(UnitResponse {}),
            Self::ErrorResponse(reason) => Pdu::ErrorResponse(ErrorResponse {
                reason: reason.clone(),
            }),
            Self::WriteToPane { pane_id, data } => Pdu::WriteToPane(WriteToPane {
                pane_id: *pane_id,
                data: data.clone(),
            }),
            Self::SendPaste { pane_id, data } => Pdu::SendPaste(SendPaste {
                pane_id: *pane_id,
                data: data.clone(),
            }),
            Self::SetClipboard {
                pane_id,
                clipboard,
                selection,
            } => Pdu::SetClipboard(SetClipboard {
                pane_id: *pane_id,
                clipboard: clipboard.clone(),
                selection: *selection,
            }),
            Self::SetWindowWorkspace {
                window_id,
                workspace,
            } => Pdu::SetWindowWorkspace(SetWindowWorkspace {
                window_id: *window_id,
                workspace: workspace.clone(),
            }),
            Self::Resize {
                containing_tab_id,
                pane_id,
                rows,
                cols,
            } => Pdu::Resize(Resize {
                containing_tab_id: *containing_tab_id,
                pane_id: *pane_id,
                size: TerminalSize {
                    rows: *rows,
                    cols: *cols,
                    pixel_width: cols.saturating_mul(10),
                    pixel_height: rows.saturating_mul(20),
                    dpi: 96,
                },
            }),
            Self::SetPaneZoomed {
                containing_tab_id,
                pane_id,
                zoomed,
            } => Pdu::SetPaneZoomed(SetPaneZoomed {
                containing_tab_id: *containing_tab_id,
                pane_id: *pane_id,
                zoomed: *zoomed,
            }),
        }
    }
}

fn arb_clipboard_selection() -> impl Strategy<Value = ClipboardSelection> {
    prop_oneof![
        Just(ClipboardSelection::Clipboard),
        Just(ClipboardSelection::PrimarySelection),
    ]
}

fn arb_wire_framing_pdu() -> impl Strategy<Value = WireFramingPdu> {
    prop_oneof![
        Just(WireFramingPdu::Ping),
        Just(WireFramingPdu::Pong),
        Just(WireFramingPdu::UnitResponse),
        arb_small_string().prop_map(WireFramingPdu::ErrorResponse),
        (
            0usize..=4096,
            proptest::collection::vec(any::<u8>(), 0..128)
        )
            .prop_map(|(pane_id, data)| WireFramingPdu::WriteToPane { pane_id, data }),
        (0usize..=4096, arb_small_string())
            .prop_map(|(pane_id, data)| WireFramingPdu::SendPaste { pane_id, data }),
        (
            0usize..=4096,
            prop::option::of(arb_small_string()),
            arb_clipboard_selection(),
        )
            .prop_map(
                |(pane_id, clipboard, selection)| WireFramingPdu::SetClipboard {
                    pane_id,
                    clipboard,
                    selection,
                }
            ),
        (0usize..=4096, arb_small_string()).prop_map(|(window_id, workspace)| {
            WireFramingPdu::SetWindowWorkspace {
                window_id,
                workspace,
            }
        }),
        (0usize..=4096, 0usize..=4096, 1usize..=80, 1usize..=200).prop_map(
            |(containing_tab_id, pane_id, rows, cols)| WireFramingPdu::Resize {
                containing_tab_id,
                pane_id,
                rows,
                cols,
            },
        ),
        (0usize..=4096, 0usize..=4096, any::<bool>()).prop_map(
            |(containing_tab_id, pane_id, zoomed)| WireFramingPdu::SetPaneZoomed {
                containing_tab_id,
                pane_id,
                zoomed,
            },
        ),
    ]
}

fn generated_terminal_size(
    rows: usize,
    cols: usize,
    pixel_width: usize,
    pixel_height: usize,
) -> TerminalSize {
    TerminalSize {
        rows,
        cols,
        pixel_width,
        pixel_height,
        dpi: 96,
    }
}

fn assert_stream_decode_preserves_trailing_bytes(
    serial: u64,
    pdu: &WireFramingPdu,
    mode: CompressionMode,
    trailing: &[u8],
) -> Result<(), TestCaseError> {
    let mut encoded = Vec::new();
    pdu.to_pdu()
        .encode_with_mode(&mut encoded, serial, mode)
        .unwrap();

    let direct = Pdu::decode(encoded.as_slice()).unwrap();
    prop_assert_eq!(direct.serial, serial);
    prop_assert_eq!(direct.pdu, pdu.to_pdu());

    let mut framed = encoded;
    framed.extend_from_slice(trailing);

    let streamed = Pdu::stream_decode(&mut framed).unwrap().unwrap();
    prop_assert_eq!(streamed.serial, serial);
    prop_assert_eq!(streamed.pdu, pdu.to_pdu());
    prop_assert_eq!(framed, trailing);
    Ok(())
}

fn tagged_len_prefix(encoded: &[u8]) -> Result<(u64, usize), TestCaseError> {
    let mut remaining = encoded;
    let tagged_len = leb128::read::unsigned(&mut remaining)
        .map_err(|err| TestCaseError::fail(format!("tagged_len decode failed: {err}")))?;
    Ok((tagged_len, encoded.len() - remaining.len()))
}

fn encode_unsigned(value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    leb128::write::unsigned(&mut out, value).expect("encode leb128");
    out
}

fn retag_frame_len(encoded: &[u8], new_tagged_len: u64) -> Result<Vec<u8>, TestCaseError> {
    let (_old_tagged_len, old_len_prefix_len) = tagged_len_prefix(encoded)?;
    let mut out = encode_unsigned(new_tagged_len);
    out.extend_from_slice(&encoded[old_len_prefix_len..]);
    Ok(out)
}

fn assert_frame_header_and_prefix_contract(
    serial: u64,
    pdu: &WireFramingPdu,
    mode: CompressionMode,
) -> Result<(), TestCaseError> {
    let pdu = pdu.to_pdu();
    let mut encoded = Vec::new();
    pdu.encode_with_mode(&mut encoded, serial, mode)
        .expect("encode_with_mode");

    let (tagged_len, tagged_len_bytes) = tagged_len_prefix(&encoded)?;
    let is_compressed = (tagged_len & COMPRESSED_MASK) != 0;
    let raw_len = tagged_len & !COMPRESSED_MASK;
    let raw_len: usize = raw_len
        .try_into()
        .map_err(|_| TestCaseError::fail("raw tagged_len does not fit usize"))?;
    prop_assert_eq!(
        tagged_len_bytes + raw_len,
        encoded.len(),
        "tagged_len must count serial+ident+payload bytes exactly"
    );

    match mode {
        CompressionMode::Always => {
            prop_assert!(is_compressed, "Always mode must set the compressed flag")
        }
        CompressionMode::Never => {
            prop_assert!(
                !is_compressed,
                "Never mode must leave the compressed flag clear"
            )
        }
        CompressionMode::Auto => {}
    }

    for split in 0..encoded.len() {
        let mut partial = encoded[..split].to_vec();
        let before = partial.clone();
        let decoded = Pdu::stream_decode(&mut partial)
            .map_err(|err| TestCaseError::fail(format!("stream_decode prefix failed: {err}")))?;
        prop_assert!(
            decoded.is_none(),
            "strict prefix of length {split} decoded as a complete frame"
        );
        prop_assert_eq!(
            partial,
            before,
            "strict prefix of length {} must remain buffered unchanged",
            split
        );
    }

    let mut complete = encoded;
    let decoded = Pdu::stream_decode(&mut complete)
        .map_err(|err| TestCaseError::fail(format!("stream_decode complete failed: {err}")))?
        .ok_or_else(|| TestCaseError::fail("complete frame did not decode"))?;
    prop_assert_eq!(decoded.serial, serial);
    prop_assert_eq!(decoded.pdu, pdu);
    prop_assert!(complete.is_empty(), "complete frame must be fully consumed");
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Roundtrip for the `MovePaneToNewTab` request PDU.
    #[test]
    fn move_pane_to_new_tab_request_json_and_pdu_roundtrip(
        pane_id in 0usize..=4096,
        window_id in prop::option::of(0usize..=4096),
        workspace_for_new_window in prop::option::of(arb_small_string()),
        serial in any::<u64>(),
    ) {
        let payload = MovePaneToNewTab {
            pane_id,
            window_id,
            workspace_for_new_window,
        };

        // JSON roundtrip exercises the serde representation directly.
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: MovePaneToNewTab = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload.clone());

        // Full PDU roundtrip through the varbincode + compression envelope.
        assert_pdu_roundtrip(serial, Pdu::MovePaneToNewTab(payload));
    }

    /// Roundtrip for `SpawnV2` with `command: Some(CommandBuilder)` — the
    /// branch the legacy `arb_spawn_v2` strategy does not exercise.
    #[test]
    fn spawn_v2_with_command_json_and_pdu_roundtrip(
        argv0 in "[a-zA-Z][a-zA-Z0-9_-]{0,8}",
        extra_args in proptest::collection::vec("[a-zA-Z0-9_.-]{0,12}", 0..4),
        env_pairs in proptest::collection::vec(
            ("[A-Z][A-Z0-9_]{0,6}", "[a-zA-Z0-9 _./-]{0,16}"),
            0..4,
        ),
        cwd in prop::option::of("[a-zA-Z0-9/_.-]{1,24}"),
        window_id in prop::option::of(0usize..=4096),
        command_dir in prop::option::of(arb_small_string()),
        workspace in arb_small_string(),
        serial in any::<u64>(),
    ) {
        // Build a CommandBuilder using only the public `env_clear` +
        // `env` + `arg` + `cwd` surface so the captured env map holds
        // only caller-supplied keys (not the test host's process env,
        // which would make the test flaky across hosts).
        let cmd = build_generated_command(&argv0, &extra_args, &env_pairs, cwd.as_ref());

        let payload = SpawnV2 {
            domain: SpawnTabDomain::DefaultDomain,
            window_id,
            command: Some(cmd),
            command_dir,
            size: TerminalSize {
                rows: 24,
                cols: 80,
                pixel_width: 800,
                pixel_height: 480,
                dpi: 96,
            },
            workspace,
        };

        // JSON roundtrip exercises the CommandBuilder serde adapter on
        // the wire path most clients use.
        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: SpawnV2 = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload.clone());

        // Full PDU roundtrip through the varbincode + compression envelope.
        assert_pdu_roundtrip(serial, Pdu::SpawnV2(payload));
    }

    /// Roundtrip for `SplitPane` with `command: Some(CommandBuilder)`.
    ///
    /// This is the other mux subprocess spawn PDU branch: the main codec
    /// strategy pins `command: None`, so generated split-spawn payloads did
    /// not exercise the PTY `CommandBuilder` wire adapter through the PDU
    /// envelope.
    #[test]
    fn split_pane_with_command_json_and_pdu_roundtrip(
        pane_id in 0usize..=4096,
        direction in prop_oneof![
            Just(SplitDirection::Horizontal),
            Just(SplitDirection::Vertical),
        ],
        target_is_second in any::<bool>(),
        top_level in any::<bool>(),
        split_size in prop_oneof![
            (0usize..=256).prop_map(SplitSize::Cells),
            (0u8..=100).prop_map(SplitSize::Percent),
        ],
        argv0 in "[a-zA-Z][a-zA-Z0-9_-]{0,8}",
        extra_args in proptest::collection::vec("[a-zA-Z0-9_.-]{0,12}", 0..4),
        env_pairs in proptest::collection::vec(
            ("[A-Z][A-Z0-9_]{0,6}", "[a-zA-Z0-9 _./-]{0,16}"),
            0..4,
        ),
        cwd in prop::option::of("[a-zA-Z0-9/_.-]{1,24}"),
        command_dir in prop::option::of(arb_small_string()),
        domain in prop_oneof![
            Just(SpawnTabDomain::DefaultDomain),
            Just(SpawnTabDomain::CurrentPaneDomain),
            "[a-zA-Z][a-zA-Z0-9_-]{0,8}".prop_map(SpawnTabDomain::DomainName),
            (0usize..=1024).prop_map(SpawnTabDomain::DomainId),
        ],
        serial in any::<u64>(),
    ) {
        let cmd = build_generated_command(&argv0, &extra_args, &env_pairs, cwd.as_ref());
        let payload = SplitPane {
            pane_id,
            split_request: SplitRequest {
                direction,
                target_is_second,
                top_level,
                size: split_size,
            },
            command: Some(cmd),
            command_dir,
            domain,
            move_pane_id: None,
        };

        let json = serde_json::to_string(&payload).unwrap();
        let decoded_json: SplitPane = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded_json, payload.clone());

        assert_pdu_roundtrip(serial, Pdu::SplitPane(payload));
    }

    /// A streaming mux reader may have already buffered bytes for the next
    /// frame. For every compression mode, decoding one complete frame must
    /// consume exactly that frame and leave all trailing bytes untouched.
    #[test]
    fn stream_decode_preserves_trailing_bytes_under_all_compression_modes(
        pdu in arb_wire_framing_pdu(),
        serial in any::<u64>(),
        trailing in proptest::collection::vec(any::<u8>(), 0..256),
    ) {
        for mode in ALL_COMPRESSION_MODES {
            assert_stream_decode_preserves_trailing_bytes(serial, &pdu, mode, &trailing)?;
        }
    }

    /// Mux dispatch can observe multiple already-buffered PDU frames in one
    /// read. A mixed stream of Auto/Never/Always frames must decode in order
    /// without letting one compression mode shift the boundary of the next.
    #[test]
    fn coalesced_pdu_stream_roundtrips_under_all_compression_modes(
        pdus in proptest::collection::vec(arb_wire_framing_pdu(), 1..24),
        serial_seed in any::<u64>(),
    ) {
        let mut wire = Vec::new();
        let mut expected = Vec::new();

        for (idx, pdu) in pdus.iter().enumerate() {
            for (mode_idx, mode) in ALL_COMPRESSION_MODES.iter().copied().enumerate() {
                let serial = serial_seed.wrapping_add((idx * ALL_COMPRESSION_MODES.len() + mode_idx) as u64);
                let decoded_pdu = pdu.to_pdu();
                decoded_pdu
                    .encode_with_mode(&mut wire, serial, mode)
                    .expect("encode_with_mode");
                expected.push((serial, decoded_pdu));
            }
        }

        for (idx, (serial, pdu)) in expected.into_iter().enumerate() {
            let decoded = Pdu::stream_decode(&mut wire)
                .expect("stream_decode")
                .unwrap_or_else(|| panic!("missing decoded frame at index {}", idx));
            prop_assert_eq!(decoded.serial, serial);
            prop_assert_eq!(decoded.pdu, pdu);
        }

        prop_assert!(
            wire.is_empty(),
            "stream_decode left {} bytes after all generated frames",
            wire.len()
        );
    }

    /// Real mux transports do not respect PDU frame boundaries: a read can
    /// split one header, one payload, or several adjacent frames arbitrarily.
    /// Feeding a rolling buffer in generated chunk sizes must still decode the
    /// original framed sequence exactly once, in order, without dropping bytes
    /// from incomplete frames.
    #[test]
    fn chunked_pdu_stream_decodes_complete_frames_in_order(
        pdus in proptest::collection::vec(arb_wire_framing_pdu(), 1..24),
        serial_seed in any::<u64>(),
        chunk_sizes in proptest::collection::vec(1usize..64, 1..64),
    ) {
        let mut wire = Vec::new();
        let mut expected = Vec::new();

        for (idx, pdu) in pdus.iter().enumerate() {
            let mode = ALL_COMPRESSION_MODES[idx % ALL_COMPRESSION_MODES.len()];
            let serial = serial_seed.wrapping_add(idx as u64);
            let decoded_pdu = pdu.to_pdu();
            decoded_pdu
                .encode_with_mode(&mut wire, serial, mode)
                .expect("encode_with_mode");
            expected.push((serial, decoded_pdu));
        }

        let mut buffer = Vec::new();
        let mut actual = Vec::new();
        let mut offset = 0usize;
        let mut chunk_iter = chunk_sizes.iter().copied().cycle();

        while offset < wire.len() {
            let chunk_len = chunk_iter.next().unwrap_or(wire.len()).min(wire.len() - offset);
            buffer.extend_from_slice(&wire[offset..offset + chunk_len]);
            offset += chunk_len;

            while let Some(decoded) = Pdu::stream_decode(&mut buffer)
                .map_err(|err| TestCaseError::fail(format!("stream_decode failed: {err}")))?
            {
                actual.push((decoded.serial, decoded.pdu));
            }
        }

        prop_assert_eq!(actual, expected);
        prop_assert!(
            buffer.is_empty(),
            "stream_decode left {} bytes after the final generated chunk",
            buffer.len()
        );
    }

    /// PDU stream readers rely on the raw frame header to distinguish
    /// incomplete reads from complete frames. Every compression mode must
    /// advertise an exact frame length, preserve incomplete prefixes, and set
    /// or clear the compressed bit according to forced-mode semantics.
    #[test]
    fn pdu_frame_headers_and_prefixes_hold_under_all_compression_modes(
        pdu in arb_wire_framing_pdu(),
        serial in any::<u64>(),
    ) {
        for mode in ALL_COMPRESSION_MODES {
            assert_frame_header_and_prefix_contract(serial, &pdu, mode)?;
        }
    }

    /// A corrupted mux frame length should never half-consume the rolling
    /// read buffer. Either a generated mutation still forms a decodable frame
    /// and consumes bytes, or `stream_decode` reports incomplete/malformed
    /// input while leaving all buffered bytes available for later handling.
    #[test]
    fn mutated_tagged_len_never_partially_consumes_on_error_or_incomplete(
        pdu in arb_wire_framing_pdu(),
        serial in any::<u64>(),
        mode in prop_oneof![
            Just(CompressionMode::Auto),
            Just(CompressionMode::Never),
            Just(CompressionMode::Always),
        ],
        raw_len_delta in -8i16..=8,
        suffix in proptest::collection::vec(any::<u8>(), 0..256),
    ) {
        let pdu = pdu.to_pdu();
        let mut encoded = Vec::new();
        pdu.encode_with_mode(&mut encoded, serial, mode)
            .expect("encode_with_mode");

        let (tagged_len, _) = tagged_len_prefix(&encoded)?;
        let compressed_bit = tagged_len & COMPRESSED_MASK;
        let raw_len = tagged_len & !COMPRESSED_MASK;
        let delta_abs = i64::from(raw_len_delta).abs() as u64;
        let mutated_raw_len = if raw_len_delta < 0 {
            raw_len.saturating_sub(delta_abs)
        } else {
            raw_len.saturating_add(delta_abs)
        };

        let mut mutated = retag_frame_len(&encoded, compressed_bit | mutated_raw_len)?;
        mutated.extend_from_slice(&suffix);
        let before = mutated.clone();

        match Pdu::stream_decode(&mut mutated) {
            Ok(Some(_decoded)) => {
                prop_assert!(
                    mutated.len() < before.len(),
                    "successful decode must consume at least one framed byte"
                );
            }
            Ok(None) => prop_assert_eq!(
                mutated,
                before,
                "incomplete mutated-length frame must leave buffered bytes unchanged"
            ),
            Err(_) => prop_assert_eq!(
                mutated,
                before,
                "malformed mutated-length frame must leave buffered bytes unchanged"
            ),
        }
    }

    /// The mux/PTY spawn handshake is a two-message IPC exchange: the client
    /// sends `SpawnV2` carrying a PTY `CommandBuilder`, and the mux server
    /// replies with `SpawnResponse`. Feed that pair through arbitrary chunk
    /// boundaries so the generated handshake proves request/response ordering
    /// and command payload preservation on the real PDU stream decoder.
    #[test]
    fn spawn_v2_pty_handshake_stream_decodes_in_order_under_chunking(
        argv0 in "[a-zA-Z][a-zA-Z0-9_-]{0,8}",
        extra_args in proptest::collection::vec("[a-zA-Z0-9_.-]{0,12}", 0..4),
        env_pairs in proptest::collection::vec(
            ("[A-Z][A-Z0-9_]{0,6}", "[a-zA-Z0-9 _./-]{0,16}"),
            0..4,
        ),
        cwd in prop::option::of("[a-zA-Z0-9/_.-]{1,24}"),
        command_dir in prop::option::of(arb_small_string()),
        workspace in arb_small_string(),
        window_id in prop::option::of(0usize..=4096),
        response_window_id in 0usize..=4096,
        tab_id in 0usize..=4096,
        pane_id in 0usize..=4096,
        rows in 1usize..=80,
        cols in 1usize..=200,
        pixel_width in 0usize..=8192,
        pixel_height in 0usize..=8192,
        serial_seed in any::<u64>(),
        request_mode in prop_oneof![
            Just(CompressionMode::Auto),
            Just(CompressionMode::Never),
            Just(CompressionMode::Always),
        ],
        response_mode in prop_oneof![
            Just(CompressionMode::Auto),
            Just(CompressionMode::Never),
            Just(CompressionMode::Always),
        ],
        chunk_sizes in proptest::collection::vec(1usize..64, 1..64),
    ) {
        let size = generated_terminal_size(rows, cols, pixel_width, pixel_height);
        let command = build_generated_command(&argv0, &extra_args, &env_pairs, cwd.as_ref());
        let request = Pdu::SpawnV2(SpawnV2 {
            domain: SpawnTabDomain::DefaultDomain,
            window_id,
            command: Some(command),
            command_dir,
            size,
            workspace,
        });
        let response = Pdu::SpawnResponse(SpawnResponse {
            pane_id,
            tab_id,
            window_id: response_window_id,
            size,
        });

        let request_serial = serial_seed;
        let response_serial = serial_seed.wrapping_add(1);
        let mut wire = Vec::new();
        request
            .encode_with_mode(&mut wire, request_serial, request_mode)
            .expect("encode SpawnV2 request");
        response
            .encode_with_mode(&mut wire, response_serial, response_mode)
            .expect("encode SpawnResponse");

        let mut buffer = Vec::new();
        let mut actual = Vec::new();
        let mut offset = 0usize;
        let mut chunk_iter = chunk_sizes.iter().copied().cycle();

        while offset < wire.len() {
            let chunk_len = chunk_iter.next().unwrap_or(wire.len()).min(wire.len() - offset);
            buffer.extend_from_slice(&wire[offset..offset + chunk_len]);
            offset += chunk_len;

            while let Some(decoded) = Pdu::stream_decode(&mut buffer)
                .map_err(|err| TestCaseError::fail(format!("handshake stream_decode failed: {err}")))?
            {
                actual.push((decoded.serial, decoded.pdu));
            }
        }

        let expected = vec![(request_serial, request), (response_serial, response)];
        prop_assert_eq!(actual, expected);
        prop_assert!(buffer.is_empty(), "handshake stream left {} bytes", buffer.len());
    }
}
