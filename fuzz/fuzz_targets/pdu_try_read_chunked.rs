#![no_main]

use codec::Pdu;
use libfuzzer_sys::fuzz_target;
use std::io::{self, Read};

const MAX_INPUT_LEN: usize = 64 * 1024;
const MAX_INITIAL_BUFFER: usize = 128;
const MAX_READ_STEPS: usize = 16;

struct ChunkedWouldBlockReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> ChunkedWouldBlockReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }
}

impl Read for ChunkedWouldBlockReader<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.bytes.len() {
            return Ok(0);
        }

        let control = self.bytes[self.pos];
        self.pos += 1;

        if (control & 0b11) == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "fuzz would-block",
            ));
        }

        if self.pos >= self.bytes.len() {
            return Ok(0);
        }

        let requested = ((control as usize) >> 2).saturating_add(1);
        let available = self.bytes.len() - self.pos;
        let len = requested.min(available).min(out.len());
        out[..len].copy_from_slice(&self.bytes[self.pos..self.pos + len]);
        self.pos += len;
        Ok(len)
    }
}

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_LEN {
        return;
    }

    let Some((&split_byte, rest)) = data.split_first() else {
        return;
    };
    let initial_len = (split_byte as usize)
        .min(MAX_INITIAL_BUFFER)
        .min(rest.len());
    let (initial, scripted_reads) = rest.split_at(initial_len);

    let mut buffer = initial.to_vec();
    let mut reader = ChunkedWouldBlockReader::new(scripted_reads);

    for _ in 0..MAX_READ_STEPS {
        match Pdu::try_read_and_decode(&mut reader, &mut buffer) {
            Ok(Some(decoded)) => {
                let _ = decoded.serial;
                let _ = decoded.pdu.pdu_name();
            }
            Ok(None) => continue,
            Err(_) => break,
        }
    }
});
