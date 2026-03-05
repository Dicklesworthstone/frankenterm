use std::collections::VecDeque;
use std::io::{self, Read};

use codec::{
    CreateFloatingPane, CycleStack, DecodedPdu, MoveFloatingPane, Pdu, Ping, RemoveFloatingPane,
    SelectStackPane, SetClipboard, SetLayoutCycle, SetPalette, SetPaneZoomed, SwapToLayout,
    ToggleFloatingPane, UnitResponse, UpdatePaneConstraints,
};
use frankenterm_term::color::ColorPalette;
use frankenterm_term::ClipboardSelection;
use mux::tab::FloatingPaneRect;

enum ReadStep {
    Data(Vec<u8>),
    WouldBlock,
}

struct ScriptedReader {
    steps: VecDeque<ReadStep>,
}

impl ScriptedReader {
    fn new(steps: Vec<ReadStep>) -> Self {
        Self {
            steps: steps.into_iter().collect(),
        }
    }
}

impl Read for ScriptedReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.steps.pop_front() {
            Some(ReadStep::Data(chunk)) => {
                let n = chunk.len().min(buf.len());
                buf[..n].copy_from_slice(&chunk[..n]);
                if n < chunk.len() {
                    self.steps.push_front(ReadStep::Data(chunk[n..].to_vec()));
                }
                Ok(n)
            }
            Some(ReadStep::WouldBlock) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "scripted would block",
            )),
            None => Ok(0),
        }
    }
}

#[test]
fn stream_decode_unknown_ident_consumes_frame() {
    // len=2, serial=1, ident=99 (unknown), no payload
    let mut buffer = vec![2, 1, 99];
    let decoded = Pdu::stream_decode(&mut buffer).unwrap().unwrap();
    assert_eq!(
        decoded,
        DecodedPdu {
            serial: 1,
            pdu: Pdu::Invalid { ident: 99 }
        }
    );
    assert!(buffer.is_empty());
}

#[test]
fn stream_decode_partial_frame_preserves_buffer() {
    let mut buffer = vec![2];
    let decoded = Pdu::stream_decode(&mut buffer).unwrap();
    assert!(decoded.is_none());
    assert_eq!(buffer, vec![2]);
}

#[test]
fn stream_decode_overflow_length_returns_error_and_preserves_buffer() {
    // Overflowing leb128 length.
    let mut buffer = vec![0x80; 10];
    buffer.push(0x02);
    let before = buffer.clone();

    assert!(Pdu::stream_decode(&mut buffer).is_err());
    assert_eq!(buffer, before);
}

#[test]
fn try_read_and_decode_would_block_without_data_returns_none() {
    let mut reader = ScriptedReader::new(vec![ReadStep::WouldBlock]);
    let mut read_buffer = Vec::new();
    let decoded = Pdu::try_read_and_decode(&mut reader, &mut read_buffer).unwrap();
    assert!(decoded.is_none());
    assert!(read_buffer.is_empty());
}

#[test]
fn try_read_and_decode_would_block_preserves_partial_buffer() {
    let mut encoded = Vec::new();
    Pdu::Ping(Ping {}).encode(&mut encoded, 7).unwrap();

    let mut reader = ScriptedReader::new(vec![ReadStep::WouldBlock]);
    let mut read_buffer = vec![encoded[0]];
    let decoded = Pdu::try_read_and_decode(&mut reader, &mut read_buffer).unwrap();

    assert!(decoded.is_none());
    assert_eq!(read_buffer, vec![encoded[0]]);
}

#[test]
fn try_read_and_decode_handles_incremental_reads() {
    let mut encoded = Vec::new();
    Pdu::Ping(Ping {}).encode(&mut encoded, 33).unwrap();

    let mut reader = ScriptedReader::new(vec![
        ReadStep::Data(vec![encoded[0]]),
        ReadStep::Data(encoded[1..].to_vec()),
    ]);
    let mut read_buffer = Vec::new();

    let decoded = Pdu::try_read_and_decode(&mut reader, &mut read_buffer)
        .unwrap()
        .unwrap();

    assert_eq!(decoded.serial, 33);
    assert_eq!(decoded.pdu, Pdu::Ping(Ping {}));
    assert!(read_buffer.is_empty());
}

#[test]
fn pane_id_for_set_clipboard_variant() {
    let pdu = Pdu::SetClipboard(SetClipboard {
        pane_id: 44,
        clipboard: Some("copied".to_string()),
        selection: ClipboardSelection::Clipboard,
    });
    assert_eq!(pdu.pane_id(), Some(44));
}

#[test]
fn pane_id_for_set_palette_variant() {
    let pdu = Pdu::SetPalette(SetPalette {
        pane_id: 55,
        palette: ColorPalette::default(),
    });
    assert_eq!(pdu.pane_id(), Some(55));
}

#[test]
fn is_user_input_for_zoom_and_non_input_response() {
    let zoom = Pdu::SetPaneZoomed(SetPaneZoomed {
        containing_tab_id: 1,
        pane_id: 2,
        zoomed: true,
    });
    assert!(zoom.is_user_input());
    assert!(!Pdu::UnitResponse(UnitResponse {}).is_user_input());
}

// --- FrankenTerm custom PDU stream decode tests (IDs 63-72) ---
// These test encode→stream_decode for the 10 custom PDUs, complementing
// the basic encode→decode roundtrip tests in codec/src/lib.rs.

#[test]
fn stream_decode_create_floating_pane() {
    let pdu = Pdu::CreateFloatingPane(CreateFloatingPane {
        tab_id: 5,
        pane_id: 42,
        rect: FloatingPaneRect {
            left: 10,
            top: 20,
            width: 60,
            height: 30,
        },
    });
    let mut encoded = Vec::new();
    pdu.encode(&mut encoded, 500).unwrap();

    let mut buffer = encoded.clone();
    let decoded = Pdu::stream_decode(&mut buffer).unwrap().unwrap();
    assert_eq!(decoded.serial, 500);
    assert_eq!(decoded.pdu, pdu);
    assert!(
        buffer.is_empty(),
        "stream_decode should consume entire frame"
    );
}

#[test]
fn stream_decode_swap_to_layout() {
    let pdu = Pdu::SwapToLayout(SwapToLayout {
        tab_id: 3,
        layout_index: 2,
    });
    let mut encoded = Vec::new();
    pdu.encode(&mut encoded, 501).unwrap();

    let mut buffer = encoded;
    let decoded = Pdu::stream_decode(&mut buffer).unwrap().unwrap();
    assert_eq!(decoded.serial, 501);
    assert_eq!(decoded.pdu, pdu);
}

#[test]
fn stream_decode_update_pane_constraints() {
    let pdu = Pdu::UpdatePaneConstraints(UpdatePaneConstraints {
        pane_id: 99,
        min_width: Some(20),
        max_width: Some(200),
        min_height: None,
        max_height: Some(50),
    });
    let mut encoded = Vec::new();
    pdu.encode(&mut encoded, 502).unwrap();

    let mut buffer = encoded;
    let decoded = Pdu::stream_decode(&mut buffer).unwrap().unwrap();
    assert_eq!(decoded.pdu, pdu);
}

#[test]
fn incremental_read_floating_pane_pdu() {
    let pdu = Pdu::MoveFloatingPane(MoveFloatingPane {
        pane_id: 7,
        rect: FloatingPaneRect {
            left: 0,
            top: 0,
            width: 80,
            height: 24,
        },
    });
    let mut encoded = Vec::new();
    pdu.encode(&mut encoded, 600).unwrap();

    // Feed one byte at a time to test incremental decode
    let steps: Vec<ReadStep> = encoded.iter().map(|&b| ReadStep::Data(vec![b])).collect();
    let mut reader = ScriptedReader::new(steps);
    let mut read_buffer = Vec::new();

    let decoded = Pdu::try_read_and_decode(&mut reader, &mut read_buffer)
        .unwrap()
        .unwrap();
    assert_eq!(decoded.serial, 600);
    assert_eq!(decoded.pdu, pdu);
}

#[test]
fn all_frankenmux_pdus_have_unique_names() {
    let pdus: Vec<Pdu> = vec![
        Pdu::CreateFloatingPane(CreateFloatingPane {
            tab_id: 0,
            pane_id: 0,
            rect: FloatingPaneRect {
                left: 0,
                top: 0,
                width: 1,
                height: 1,
            },
        }),
        Pdu::MoveFloatingPane(MoveFloatingPane {
            pane_id: 0,
            rect: FloatingPaneRect {
                left: 0,
                top: 0,
                width: 1,
                height: 1,
            },
        }),
        Pdu::SetFloatingPaneZ(codec::SetFloatingPaneZ {
            pane_id: 0,
            z_order: 0,
        }),
        Pdu::ToggleFloatingPane(ToggleFloatingPane {
            pane_id: 0,
            visible: true,
        }),
        Pdu::RemoveFloatingPane(RemoveFloatingPane { pane_id: 0 }),
        Pdu::SwapToLayout(SwapToLayout {
            tab_id: 0,
            layout_index: 0,
        }),
        Pdu::SetLayoutCycle(SetLayoutCycle {
            tab_id: 0,
            layout_names: vec![],
        }),
        Pdu::CycleStack(CycleStack {
            tab_id: 0,
            slot_index: 0,
            forward: true,
        }),
        Pdu::SelectStackPane(SelectStackPane {
            tab_id: 0,
            slot_index: 0,
            pane_index: 0,
        }),
        Pdu::UpdatePaneConstraints(UpdatePaneConstraints {
            pane_id: 0,
            min_width: None,
            max_width: None,
            min_height: None,
            max_height: None,
        }),
    ];

    let names: Vec<&str> = pdus.iter().map(|p| p.pdu_name()).collect();
    let unique: std::collections::HashSet<&str> = names.iter().copied().collect();
    assert_eq!(
        names.len(),
        unique.len(),
        "All 10 FrankenTerm PDUs should have unique names"
    );

    // Verify expected names
    assert!(unique.contains("CreateFloatingPane"));
    assert!(unique.contains("MoveFloatingPane"));
    assert!(unique.contains("SetFloatingPaneZ"));
    assert!(unique.contains("ToggleFloatingPane"));
    assert!(unique.contains("RemoveFloatingPane"));
    assert!(unique.contains("SwapToLayout"));
    assert!(unique.contains("SetLayoutCycle"));
    assert!(unique.contains("CycleStack"));
    assert!(unique.contains("SelectStackPane"));
    assert!(unique.contains("UpdatePaneConstraints"));
}

#[test]
fn set_layout_cycle_with_empty_names() {
    // Edge case: empty layout_names should still roundtrip
    let pdu = Pdu::SetLayoutCycle(SetLayoutCycle {
        tab_id: 1,
        layout_names: vec![],
    });
    let mut encoded = Vec::new();
    pdu.encode(&mut encoded, 700).unwrap();
    let decoded = Pdu::decode(encoded.as_slice()).unwrap();
    assert_eq!(decoded.pdu, pdu);
}

#[test]
fn update_pane_constraints_all_none() {
    // Edge case: all constraint fields None
    let pdu = Pdu::UpdatePaneConstraints(UpdatePaneConstraints {
        pane_id: 1,
        min_width: None,
        max_width: None,
        min_height: None,
        max_height: None,
    });
    let mut encoded = Vec::new();
    pdu.encode(&mut encoded, 701).unwrap();
    let decoded = Pdu::decode(encoded.as_slice()).unwrap();
    assert_eq!(decoded.pdu, pdu);
}

#[test]
fn update_pane_constraints_all_some() {
    // All constraint fields set
    let pdu = Pdu::UpdatePaneConstraints(UpdatePaneConstraints {
        pane_id: 42,
        min_width: Some(10),
        max_width: Some(200),
        min_height: Some(5),
        max_height: Some(100),
    });
    let mut encoded = Vec::new();
    pdu.encode(&mut encoded, 702).unwrap();
    let decoded = Pdu::decode(encoded.as_slice()).unwrap();
    assert_eq!(decoded.pdu, pdu);
}

#[test]
fn cycle_stack_forward_and_backward_roundtrip() {
    for forward in [true, false] {
        let pdu = Pdu::CycleStack(CycleStack {
            tab_id: 7,
            slot_index: 3,
            forward,
        });
        let mut encoded = Vec::new();
        pdu.encode(&mut encoded, 800).unwrap();

        let mut buffer = encoded;
        let decoded = Pdu::stream_decode(&mut buffer).unwrap().unwrap();
        assert_eq!(decoded.pdu, pdu);
    }
}
