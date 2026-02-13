use std::collections::VecDeque;
use std::io::{self, Read};

use codec::{DecodedPdu, Pdu, Ping, SetClipboard, SetPalette, SetPaneZoomed, UnitResponse};
use frankenterm_term::color::ColorPalette;
use frankenterm_term::ClipboardSelection;

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
