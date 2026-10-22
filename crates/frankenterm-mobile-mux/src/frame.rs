//! Mux frames: `leb128 tagged_len`, `leb128 serial`, `leb128 ident`, body.
//! The length counts serial, ident and body; its top bit marks a
//! zstd-compressed body. Requests from the phone are never compressed.

use crate::Error;

const COMPRESSED: u64 = 1 << 63;
/// A frame or decompressed body larger than this ends the session rather
/// than being buffered: the biggest legitimate v46 reply (a full screen of
/// lines) is far smaller.
pub const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// One complete frame with its body decompressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub serial: u64,
    pub ident: u64,
    pub body: Vec<u8>,
}

/// Frames one uncompressed request.
#[must_use]
pub fn encode(ident: u64, serial: u64, body: &[u8]) -> Vec<u8> {
    let mut head = Vec::with_capacity(32);
    let mut ids = Vec::with_capacity(20);
    write_leb(&mut ids, serial);
    write_leb(&mut ids, ident);
    write_leb(&mut head, (ids.len() + body.len()) as u64);
    head.extend_from_slice(&ids);
    head.extend_from_slice(body);
    head
}

fn write_leb(out: &mut Vec<u8>, value: u64) {
    // Writing into a Vec cannot fail.
    let _ = leb128::write::unsigned(out, value);
}

/// Reads one LEB128 value from the front of `bytes`: `Ok(None)` while it
/// is still incomplete.
fn read_leb(bytes: &mut &[u8]) -> Result<Option<u64>, Error> {
    let mut probe = *bytes;
    match leb128::read::unsigned(&mut probe) {
        Ok(value) => {
            *bytes = probe;
            Ok(Some(value))
        }
        Err(leb128::read::Error::IoError(_)) => Ok(None),
        Err(leb128::read::Error::Overflow) => Err(Error::Frame("length overflow")),
    }
}

/// Collects channel bytes and yields whole frames as they complete.
#[derive(Debug, Default)]
pub struct FrameReader {
    buffer: Vec<u8>,
}

impl FrameReader {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// The next complete frame, or `None` until more bytes arrive.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, Error> {
        let mut cursor = self.buffer.as_slice();
        let Some(tagged) = read_leb(&mut cursor)? else {
            return Ok(None);
        };
        let compressed = tagged & COMPRESSED != 0;
        let length =
            usize::try_from(tagged & !COMPRESSED).map_err(|_| Error::Frame("length overflow"))?;
        if length > MAX_BODY_BYTES {
            return Err(Error::Frame("frame too large"));
        }
        if cursor.len() < length {
            return Ok(None);
        }
        let mut region = &cursor[..length];
        let consumed = self.buffer.len() - cursor.len() + length;
        let serial = read_leb(&mut region)?.ok_or(Error::Frame("truncated serial"))?;
        let ident = read_leb(&mut region)?.ok_or(Error::Frame("truncated ident"))?;
        let body = if compressed {
            zstd::bulk::Decompressor::new()
                .and_then(|mut decompressor| decompressor.decompress(region, MAX_BODY_BYTES))
                .map_err(|_| Error::Frame("undecodable compressed body"))?
        } else {
            region.to_vec()
        };
        self.buffer.drain(..consumed);
        Ok(Some(Frame {
            serial,
            ident,
            body,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_wait_for_their_last_byte() {
        let frame = encode(26, 1, &[]);
        assert_eq!(frame, [2, 1, 26]);
        let bytes = [encode(24, 300, &[7, 8, 9]), encode(10, 301, &[])].concat();
        let mut reader = FrameReader::default();
        for (index, byte) in bytes.iter().enumerate() {
            reader.push(&[*byte]);
            let first_complete = index == encode(24, 300, &[7, 8, 9]).len() - 1;
            let got = reader.next_frame().expect("valid prefix");
            assert_eq!(
                got.is_some(),
                first_complete || index == bytes.len() - 1,
                "byte {index}"
            );
            if let Some(frame) = got {
                assert!(frame.ident == 24 || frame.ident == 10);
            }
        }
    }

    #[test]
    fn a_compressed_body_is_expanded() {
        let body = vec![b'x'; 4096];
        let packed = zstd::bulk::compress(&body, 3).expect("compress");
        let mut ids = Vec::new();
        write_leb(&mut ids, 5);
        write_leb(&mut ids, 25);
        let mut frame = Vec::new();
        write_leb(&mut frame, (ids.len() + packed.len()) as u64 | COMPRESSED);
        frame.extend_from_slice(&ids);
        frame.extend_from_slice(&packed);
        let mut reader = FrameReader::default();
        reader.push(&frame);
        let got = reader.next_frame().expect("frame").expect("complete");
        assert_eq!((got.serial, got.ident, got.body), (5, 25, body));
    }

    #[test]
    fn an_oversized_frame_is_refused_before_it_is_buffered() {
        let mut frame = Vec::new();
        write_leb(&mut frame, (MAX_BODY_BYTES + 1) as u64);
        let mut reader = FrameReader::default();
        reader.push(&frame);
        assert!(reader.next_frame().is_err());
    }
}
