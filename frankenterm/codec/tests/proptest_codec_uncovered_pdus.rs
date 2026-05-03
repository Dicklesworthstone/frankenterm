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
    CompressionMode, ErrorResponse, MovePaneToNewTab, Pdu, Ping, Pong, SpawnV2, UnitResponse,
    WriteToPane,
};
use config::keyassignment::SpawnTabDomain;
use frankenterm_term::TerminalSize;
use portable_pty::CommandBuilder;
use proptest::prelude::*;

fn arb_small_string() -> impl Strategy<Value = String> {
    proptest::collection::vec(any::<char>(), 0..16).prop_map(|chars| chars.into_iter().collect())
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
    WriteToPane { pane_id: usize, data: Vec<u8> },
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
        }
    }
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
    ]
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
        let mut cmd = CommandBuilder::new(&argv0);
        cmd.env_clear();
        for arg in &extra_args {
            cmd.arg(arg);
        }
        for (k, v) in &env_pairs {
            cmd.env(k, v);
        }
        if let Some(ref dir) = cwd {
            cmd.cwd(dir);
        }

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

    /// A streaming mux reader may have already buffered bytes for the next
    /// frame. For every compression mode, decoding one complete frame must
    /// consume exactly that frame and leave all trailing bytes untouched.
    #[test]
    fn stream_decode_preserves_trailing_bytes_under_all_compression_modes(
        pdu in arb_wire_framing_pdu(),
        serial in any::<u64>(),
        trailing in proptest::collection::vec(any::<u8>(), 0..256),
    ) {
        assert_stream_decode_preserves_trailing_bytes(serial, &pdu, CompressionMode::Auto, &trailing)?;
        assert_stream_decode_preserves_trailing_bytes(serial, &pdu, CompressionMode::Never, &trailing)?;
        assert_stream_decode_preserves_trailing_bytes(serial, &pdu, CompressionMode::Always, &trailing)?;
    }
}
