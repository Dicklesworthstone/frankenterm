//! Mux frames: `leb128 tagged_len`, `leb128 serial`, `leb128 ident`, body.
//! The length counts serial, ident and body; its top bit marks a
//! zstd-compressed body. Requests from the phone are never compressed.

use crate::Error;
use std::io::Read as _;

const COMPRESSED: u64 = 1 << 63;
/// A frame larger than this ends the session rather than being buffered:
/// the biggest legitimate v46 reply (a full screen of lines) is far
/// smaller. A compressed body that expands past it is dropped instead, and
/// only the request it answers fails (a search matching nearly every cell).
pub const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Consumed bytes the reader keeps before moving the rest to the front.
const COMPACT_AFTER: usize = 64 * 1024;

/// One complete frame with its body decompressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub serial: u64,
    pub ident: u64,
    pub body: Vec<u8>,
    /// The body expanded past [`MAX_BODY_BYTES`] and was dropped (`body`
    /// is empty).
    pub oversized: bool,
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
    /// Bytes at the front of `buffer` already returned as frames: a burst
    /// of many frames is consumed without moving the rest for each one.
    start: usize,
}

impl FrameReader {
    pub fn push(&mut self, bytes: &[u8]) {
        if self.start > 0 && (self.start >= COMPACT_AFTER || self.start == self.buffer.len()) {
            self.buffer.drain(..self.start);
            self.start = 0;
        }
        self.buffer.extend_from_slice(bytes);
    }

    /// The next complete frame, or `None` until more bytes arrive.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, Error> {
        let unread = &self.buffer[self.start..];
        let mut cursor = unread;
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
        let consumed = unread.len() - cursor.len() + length;
        let serial = read_leb(&mut region)?.ok_or(Error::Frame("truncated serial"))?;
        let ident = read_leb(&mut region)?.ok_or(Error::Frame("truncated ident"))?;
        let (body, oversized) = if compressed {
            expand(region)?
        } else {
            (region.to_vec(), false)
        };
        self.start += consumed;
        Ok(Some(Frame {
            serial,
            ident,
            body,
            oversized,
        }))
    }
}

/// A zstd body, grown as it decodes (its size is not known up front) and
/// abandoned once it passes [`MAX_BODY_BYTES`].
fn expand(packed: &[u8]) -> Result<(Vec<u8>, bool), Error> {
    let undecodable = |_| Error::Frame("undecodable compressed body");
    let decoder = zstd::stream::read::Decoder::with_buffer(packed).map_err(undecodable)?;
    let mut body = Vec::new();
    decoder
        .take(MAX_BODY_BYTES as u64 + 1)
        .read_to_end(&mut body)
        .map_err(undecodable)?;
    if body.len() > MAX_BODY_BYTES {
        return Ok((Vec::new(), true));
    }
    Ok((body, false))
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
        assert_eq!((got.serial, got.ident, got.body, got.oversized), (5, 25, body, false));
    }

    #[test]
    fn a_body_that_expands_too_far_is_dropped_but_the_stream_continues() {
        let body = vec![0u8; MAX_BODY_BYTES + 1];
        let packed = zstd::bulk::compress(&body, 3).expect("compress");
        let mut ids = Vec::new();
        write_leb(&mut ids, 7);
        write_leb(&mut ids, 32);
        let mut bytes = Vec::new();
        write_leb(&mut bytes, (ids.len() + packed.len()) as u64 | COMPRESSED);
        bytes.extend_from_slice(&ids);
        bytes.extend_from_slice(&packed);
        bytes.extend_from_slice(&encode(10, 8, &[]));
        let mut reader = FrameReader::default();
        reader.push(&bytes);
        let got = reader.next_frame().expect("frame").expect("complete");
        assert_eq!((got.serial, got.ident, got.oversized), (7, 32, true));
        assert!(got.body.is_empty());
        let next = reader.next_frame().expect("frame").expect("complete");
        assert_eq!((next.serial, next.ident, next.oversized), (8, 10, false));
    }

    #[test]
    fn many_frames_in_one_burst_all_arrive() {
        let bytes: Vec<u8> = (1..=5000u64)
            .flat_map(|serial| encode(10, serial, &[1, 2, 3]))
            .collect();
        let mut reader = FrameReader::default();
        let mut serials = Vec::new();
        // Chunks that split frames, read between pushes so the consumed
        // front is compacted along the way.
        for chunk in bytes.chunks(7_001) {
            reader.push(chunk);
            while let Some(frame) = reader.next_frame().expect("frame") {
                serials.push(frame.serial);
            }
        }
        assert_eq!(serials, (1..=5000).collect::<Vec<_>>());
        reader.push(&encode(10, 5001, &[]));
        assert_eq!(reader.next_frame().unwrap().unwrap().serial, 5001);
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
