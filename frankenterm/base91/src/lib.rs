//! Implements basE91 encoding; see http://base91.sourceforge.net/
//! basE91 is an advanced method for encoding binary data as ASCII characters. It is similar to
//! UUencode or base64, but is more efficient. The overhead produced by basE91 depends on the input
//! data. It amounts at most to 23% (versus 33% for base64) and can range down to 14%, which
//! typically occurs on 0-byte blocks. This makes basE91 very useful for transferring larger files
//! over binary unsafe connections like e-mail or terminal lines.

// This Rust implementation was made by Wez Furlong based on C code that is:
// Copyright (c) 2000-2006 Joachim Henke
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are met:
//
//  - Redistributions of source code must retain the above copyright notice,
//    this list of conditions and the following disclaimer.
//  - Redistributions in binary form must reproduce the above copyright notice,
//    this list of conditions and the following disclaimer in the documentation
//    and/or other materials provided with the distribution.
//  - Neither the name of Joachim Henke nor the names of his contributors may
//    be used to endorse or promote products derived from this software without
//    specific prior written permission.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
// AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
// IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
// ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT OWNER OR CONTRIBUTORS BE
// LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR
// CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF
// SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS
// INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN
// CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE)
// ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE
// POSSIBILITY OF SUCH DAMAGE.

use std::io::Write;

const ENCTAB: [u8; 91] = *b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!#$%&()*+,./:;<=>?@[]^_`{|}~\"";

/// An invalid mapping; used to represent positions in DECTAB that have no valid
/// representation in the original input data.  These are skipped; this accomodates
/// breaking the data in eg: whitespace separated lines.
const INV: u8 = 91;
const DECTAB: [u8; 256] = [
    INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV,
    INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, 62, 90, 63, 64, 65, 66,
    INV, 67, 68, 69, 70, 71, INV, 72, 73, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 74, 75, 76, 77,
    78, 79, 80, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22,
    23, 24, 25, 81, INV, 82, 83, 84, 85, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39,
    40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 86, 87, 88, 89, INV, INV, INV, INV, INV, INV,
    INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV,
    INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV,
    INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV,
    INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV,
    INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV,
    INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV, INV,
    INV, INV, INV, INV, INV, INV, INV, INV, INV,
];

/// `Base91Encoder` wraps an impl of `std::io::Write` and does itself impl `std::io::Write`,
/// and performs a base91 encode operation on the bytes that are written to it.
/// It is important to remember to `flush` the writer at end of the data, as the encoder
/// maintains up to 2 bytes of pending data; the Drop impl will implicitly flush on
/// your behalf, but will mask any error that may occur during the flush.
pub struct Base91Encoder<'a> {
    writer: &'a mut dyn Write,
    accumulator: u64,
    bits: u32,
}

impl<'a> Base91Encoder<'a> {
    /// Construct a Base91Encoder that writes encoded data to the supplied writer
    pub fn new(writer: &'a mut dyn Write) -> Self {
        Self {
            writer,
            accumulator: 0,
            bits: 0,
        }
    }
}

impl<'a> Drop for Base91Encoder<'a> {
    fn drop(&mut self) {
        self.flush().ok();
    }
}

impl<'a> std::io::Write for Base91Encoder<'a> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        for b in buf {
            self.accumulator |= u64::from(*b) << self.bits;
            self.bits += 8;

            if self.bits > 13 {
                let val = self.accumulator & 8191;

                let val = if val > 88 {
                    self.accumulator >>= 13;
                    self.bits -= 13;
                    val as usize
                } else {
                    // We can take 14 bits
                    let val = self.accumulator & 16383;
                    self.accumulator >>= 14;
                    self.bits -= 14;
                    val as usize
                };

                let out: [u8; 2] = [ENCTAB[val % 91], ENCTAB[val / 91]];
                self.writer.write_all(&out)?;
            }
        }

        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.bits > 0 {
            let val = self.accumulator as usize;
            if self.bits > 7 || self.accumulator > 90 {
                let out: [u8; 2] = [ENCTAB[val % 91], ENCTAB[val / 91]];
                self.writer.write_all(&out)?;
            } else {
                let out: [u8; 1] = [ENCTAB[val % 91]];
                self.writer.write_all(&out)?;
            }
        }
        self.bits = 0;
        self.accumulator = 0;
        self.writer.flush()
    }
}

/// A convenience function that wraps Base91Encoder; it encodes a slice of data
/// and returns a vector holding the base91 encoded data.
pub fn encode(buf: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity((buf.len() * 123) / 100);
    {
        let mut writer = Base91Encoder::new(&mut result);
        writer.write_all(buf).unwrap();
        writer.flush().unwrap();
    }
    result
}

/// `Base91Decoder` wraps an impl of `std::io::Write` and does itself impl `std::io::Write`,
/// and performs a base91 decode operation on the bytes that are written to it.
/// It is important to remember to `flush` the writer at end of the data, as the encoder
/// maintains up to 1 byte of pending data; the Drop impl will implicitly flush on
/// your behalf, but will mask any error that may occur during the flush.
pub struct Base91Decoder<'a> {
    writer: &'a mut dyn Write,
    accumulator: u64,
    bits: u32,
    value: Option<u8>,
}

impl<'a> Base91Decoder<'a> {
    pub fn new(writer: &'a mut dyn Write) -> Self {
        Self {
            writer,
            accumulator: 0,
            bits: 0,
            value: None,
        }
    }
}

impl<'a> Drop for Base91Decoder<'a> {
    fn drop(&mut self) {
        self.flush().ok();
    }
}

impl<'a> std::io::Write for Base91Decoder<'a> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        for b in buf {
            let d = DECTAB[usize::from(*b)];

            if d == INV {
                // non-alphabet; skip
                continue;
            }

            if let Some(value) = self.value.take() {
                let value = (value as u64) + (d as u64) * 91;
                self.accumulator |= value << self.bits;
                self.bits += if (value & 8191) > 88 { 13 } else { 14 };

                loop {
                    let out: [u8; 1] = [(self.accumulator & 0xff) as u8];
                    self.writer.write_all(&out)?;
                    self.accumulator >>= 8;
                    self.bits -= 8;

                    if self.bits < 8 {
                        break;
                    }
                }
            } else {
                // Starting next value
                self.value = Some(d);
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if let Some(value) = self.value.take() {
            let out: [u8; 1] = [(self.accumulator & 0xff) as u8 | (value << self.bits)];
            self.writer.write_all(&out)?;
        }
        self.bits = 0;
        self.accumulator = 0;
        self.writer.flush()
    }
}

/// A convenience function that wraps Base91Decoder; it decodes a slice of data
/// and returns a vector holding the unencoded binary data.
pub fn decode(buf: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(buf.len());
    {
        let mut writer = Base91Decoder::new(&mut result);
        writer.write_all(buf).unwrap();
        writer.flush().unwrap();
    }
    result
}

#[cfg(test)]
mod test {
    use super::*;
    #[test]
    fn test() {
        assert_eq!(encode(b"hello\n"), b"TPwJh>UA");
        assert_eq!(decode(b"TPwJh>UA"), b"hello\n");
    }

    #[test]
    fn test_bin() {
        for reps in 0..=4 {
            let mut bin = Vec::with_capacity(256);
            for i in 0..=255u8 {
                for _ in 0..reps {
                    bin.push(i);
                }
            }

            let encoded = encode(&bin);
            eprintln!("encoded as {}", String::from_utf8(encoded.clone()).unwrap());
            let decoded = decode(&encoded);

            assert_eq!(decoded, bin);
        }
    }

    // ── Empty input ────────────────────────────────────────────

    #[test]
    fn encode_empty() {
        assert_eq!(encode(b""), b"");
    }

    #[test]
    fn decode_empty() {
        assert_eq!(decode(b""), b"");
    }

    // ── Single byte ────────────────────────────────────────────

    #[test]
    fn roundtrip_single_byte() {
        for b in 0..=255u8 {
            let encoded = encode(&[b]);
            let decoded = decode(&encoded);
            assert_eq!(decoded, vec![b], "failed roundtrip for byte {b}");
        }
    }

    // ── Various lengths ────────────────────────────────────────

    #[test]
    fn roundtrip_two_bytes() {
        let data = b"AB";
        assert_eq!(decode(&encode(data)), data);
    }

    #[test]
    fn roundtrip_three_bytes() {
        let data = b"XYZ";
        assert_eq!(decode(&encode(data)), data);
    }

    #[test]
    fn roundtrip_various_lengths() {
        for len in 0..=64 {
            let data: Vec<u8> = (0..len).map(|i| (i * 7 + 13) as u8).collect();
            let encoded = encode(&data);
            let decoded = decode(&encoded);
            assert_eq!(decoded, data, "failed roundtrip for length {len}");
        }
    }

    // ── Efficiency ─────────────────────────────────────────────

    #[test]
    fn encoding_overhead_within_spec() {
        // basE91 overhead is at most 23%
        let data: Vec<u8> = (0..1000).map(|i| (i % 256) as u8).collect();
        let encoded = encode(&data);
        let overhead = (encoded.len() as f64 / data.len() as f64 - 1.0) * 100.0;
        assert!(overhead <= 23.5, "overhead exceeds spec limit of 23%");
    }

    #[test]
    fn encoded_is_ascii() {
        let data: Vec<u8> = (0..256).map(|i| i as u8).collect();
        let encoded = encode(&data);
        for &b in &encoded {
            assert!(b.is_ascii(), "non-ASCII byte in encoded output");
        }
    }

    // ── Streaming encode ───────────────────────────────────────

    #[test]
    fn streaming_encode_matches_bulk() {
        let data = b"Hello, World! This is a test of streaming base91 encoding.";
        let bulk = encode(data);

        // Write in chunks of various sizes
        let mut result = Vec::new();
        {
            let mut encoder = Base91Encoder::new(&mut result);
            encoder.write_all(&data[..5]).unwrap();
            encoder.write_all(&data[5..20]).unwrap();
            encoder.write_all(&data[20..]).unwrap();
            encoder.flush().unwrap();
        }
        assert_eq!(result, bulk);
    }

    #[test]
    fn streaming_decode_matches_bulk() {
        let encoded = encode(b"streaming decode test data");
        let bulk = decode(&encoded);

        let mut result = Vec::new();
        {
            let mut decoder = Base91Decoder::new(&mut result);
            decoder.write_all(&encoded[..3]).unwrap();
            decoder.write_all(&encoded[3..10]).unwrap();
            decoder.write_all(&encoded[10..]).unwrap();
            decoder.flush().unwrap();
        }
        assert_eq!(result, bulk);
    }

    // ── Decoder skips invalid chars ────────────────────────────

    #[test]
    fn decode_skips_whitespace() {
        let encoded = encode(b"test");
        let encoded_str = String::from_utf8(encoded.clone()).unwrap();

        // Insert spaces and newlines
        let with_spaces: String = encoded_str.chars().map(|c| format!("{c} ")).collect();
        let decoded = decode(with_spaces.as_bytes());
        assert_eq!(decoded, b"test");
    }

    #[test]
    fn decode_skips_newlines() {
        let encoded = encode(b"newline test");
        let encoded_str = String::from_utf8(encoded).unwrap();

        // Split into lines
        let mid = encoded_str.len() / 2;
        let with_newline = format!("{}\n{}", &encoded_str[..mid], &encoded_str[mid..]);
        let decoded = decode(with_newline.as_bytes());
        assert_eq!(decoded, b"newline test");
    }

    // ── Flush behavior ─────────────────────────────────────────

    #[test]
    fn double_flush_is_safe() {
        let data = b"flush test";
        let mut result = Vec::new();
        {
            let mut encoder = Base91Encoder::new(&mut result);
            encoder.write_all(data).unwrap();
            encoder.flush().unwrap();
            encoder.flush().unwrap(); // Second flush should be a no-op
        }
        assert_eq!(decode(&result), data);
    }

    #[test]
    fn drop_flushes_encoder() {
        let data = b"drop test";
        let mut result = Vec::new();
        {
            let mut encoder = Base91Encoder::new(&mut result);
            encoder.write_all(data).unwrap();
            // Don't explicitly flush — Drop should handle it
        }
        assert_eq!(decode(&result), data);
    }

    #[test]
    fn drop_flushes_decoder() {
        let encoded = encode(b"drop decode");
        let mut result = Vec::new();
        {
            let mut decoder = Base91Decoder::new(&mut result);
            decoder.write_all(&encoded).unwrap();
            // Don't explicitly flush — Drop should handle it
        }
        assert_eq!(result, b"drop decode");
    }

    // ── Known values ───────────────────────────────────────────

    #[test]
    fn encode_known_hello() {
        assert_eq!(encode(b"hello\n"), b"TPwJh>UA");
    }

    #[test]
    fn decode_known_hello() {
        assert_eq!(decode(b"TPwJh>UA"), b"hello\n");
    }

    // ── All zeros / all ones ───────────────────────────────────

    #[test]
    fn roundtrip_all_zeros() {
        let data = vec![0u8; 100];
        assert_eq!(decode(&encode(&data)), data);
    }

    #[test]
    fn roundtrip_all_ones() {
        let data = vec![0xFFu8; 100];
        assert_eq!(decode(&encode(&data)), data);
    }

    // ── Write returns correct count ────────────────────────────

    #[test]
    fn encoder_write_returns_input_length() {
        let mut result = Vec::new();
        let mut encoder = Base91Encoder::new(&mut result);
        assert_eq!(encoder.write(b"12345").unwrap(), 5);
        assert_eq!(encoder.write(b"").unwrap(), 0);
        assert_eq!(encoder.write(b"x").unwrap(), 1);
        encoder.flush().unwrap();
    }

    #[test]
    fn decoder_write_returns_input_length() {
        let mut result = Vec::new();
        let mut decoder = Base91Decoder::new(&mut result);
        assert_eq!(decoder.write(b"TPwJh>UA").unwrap(), 8);
        assert_eq!(decoder.write(b"").unwrap(), 0);
        decoder.flush().unwrap();
    }

    // ── Determinism ───────────────────────────────────────────

    #[test]
    fn encode_is_deterministic() {
        let data = b"deterministic encoding test";
        let enc1 = encode(data);
        let enc2 = encode(data);
        assert_eq!(enc1, enc2);
    }

    // ── Byte-at-a-time streaming ──────────────────────────────

    #[test]
    fn byte_at_a_time_encode_matches_bulk() {
        let data = b"byte at a time";
        let bulk = encode(data);

        let mut result = Vec::new();
        {
            let mut encoder = Base91Encoder::new(&mut result);
            for &b in data.iter() {
                encoder.write_all(&[b]).unwrap();
            }
            encoder.flush().unwrap();
        }
        assert_eq!(result, bulk);
    }

    #[test]
    fn byte_at_a_time_decode_matches_bulk() {
        let encoded = encode(b"byte by byte decode");
        let bulk = decode(&encoded);

        let mut result = Vec::new();
        {
            let mut decoder = Base91Decoder::new(&mut result);
            for &b in encoded.iter() {
                decoder.write_all(&[b]).unwrap();
            }
            decoder.flush().unwrap();
        }
        assert_eq!(result, bulk);
    }

    // ── ENCTAB / DECTAB consistency ───────────────────────────

    #[test]
    fn enctab_has_91_unique_entries() {
        let mut seen = std::collections::HashSet::new();
        for &b in &ENCTAB {
            seen.insert(b);
        }
        assert_eq!(seen.len(), 91);
    }

    #[test]
    fn enctab_all_ascii() {
        for &b in &ENCTAB {
            assert!(b.is_ascii(), "ENCTAB contains non-ASCII byte: {b}");
        }
    }

    #[test]
    fn dectab_inverts_enctab() {
        for (i, &enc_byte) in ENCTAB.iter().enumerate() {
            let decoded = DECTAB[enc_byte as usize];
            assert_eq!(
                decoded, i as u8,
                "DECTAB[ENCTAB[{i}]] = {decoded}, expected {i}"
            );
        }
    }

    // ── Decode with only non-alphabet chars ───────────────────

    #[test]
    fn decode_only_invalid_chars_returns_empty() {
        let result = decode(b"\x00\x01\x02\t\r\n ");
        assert!(result.is_empty());
    }

    #[test]
    fn decode_tabs_skipped() {
        let encoded = encode(b"tabs");
        let encoded_str = String::from_utf8(encoded).unwrap();
        let with_tabs = encoded_str
            .replace("", "")
            .chars()
            .enumerate()
            .map(|(i, c)| {
                if i % 2 == 1 {
                    format!("\t{c}")
                } else {
                    format!("{c}")
                }
            })
            .collect::<String>();
        assert_eq!(decode(with_tabs.as_bytes()), b"tabs");
    }

    // ── Specific data patterns ────────────────────────────────

    #[test]
    fn roundtrip_alternating_bytes() {
        let data: Vec<u8> = (0..100)
            .map(|i| if i % 2 == 0 { 0xAA } else { 0x55 })
            .collect();
        assert_eq!(decode(&encode(&data)), data);
    }

    #[test]
    fn roundtrip_ascending_bytes() {
        let data: Vec<u8> = (0..=255).collect();
        assert_eq!(decode(&encode(&data)), data);
    }

    #[test]
    fn roundtrip_descending_bytes() {
        let data: Vec<u8> = (0..=255u8).rev().collect();
        assert_eq!(decode(&encode(&data)), data);
    }

    #[test]
    fn roundtrip_repeated_pattern() {
        let data: Vec<u8> = vec![0xDE, 0xAD, 0xBE, 0xEF].repeat(50);
        assert_eq!(decode(&encode(&data)), data);
    }

    // ── Larger data ───────────────────────────────────────────

    #[test]
    fn roundtrip_1kb() {
        let data: Vec<u8> = (0..1024).map(|i| (i % 256) as u8).collect();
        assert_eq!(decode(&encode(&data)), data);
    }

    #[test]
    fn roundtrip_10kb() {
        let data: Vec<u8> = (0..10240).map(|i| (i * 37 % 256) as u8).collect();
        assert_eq!(decode(&encode(&data)), data);
    }

    // ── Zero-byte efficiency (best case) ──────────────────────

    #[test]
    fn zero_block_has_low_overhead() {
        // basE91 spec says 0-byte blocks have ~14% overhead
        let data = vec![0u8; 1000];
        let encoded = encode(&data);
        let overhead = (encoded.len() as f64 / data.len() as f64 - 1.0) * 100.0;
        assert!(
            overhead < 16.0,
            "zero-block overhead {overhead:.1}% exceeds expected ~14%"
        );
    }

    // ── Flush clears state ────────────────────────────────────

    #[test]
    fn encoder_flush_resets_internal_state() {
        // After flush, encoder should have zero bits/accumulator,
        // meaning the next write starts a fresh encoding
        let mut result1 = Vec::new();
        {
            let mut encoder = Base91Encoder::new(&mut result1);
            encoder.write_all(b"hello").unwrap();
            encoder.flush().unwrap();
        }
        // Encoding the same data again should produce the same output
        let mut result2 = Vec::new();
        {
            let mut encoder = Base91Encoder::new(&mut result2);
            encoder.write_all(b"hello").unwrap();
            encoder.flush().unwrap();
        }
        assert_eq!(result1, result2);
    }

    #[test]
    fn decoder_flush_clears_state() {
        let enc1 = encode(b"part1");
        let enc2 = encode(b"part2");
        let mut result = Vec::new();
        {
            let mut decoder = Base91Decoder::new(&mut result);
            decoder.write_all(&enc1).unwrap();
            decoder.flush().unwrap();
            decoder.write_all(&enc2).unwrap();
            decoder.flush().unwrap();
        }
        assert_eq!(result, b"part1part2");
    }

    // ── Encoding output contains no control characters ────────

    #[test]
    fn encoded_contains_no_control_chars() {
        let data: Vec<u8> = (0..512).map(|i| (i % 256) as u8).collect();
        let encoded = encode(&data);
        for &b in &encoded {
            assert!(
                b >= 0x21 && b <= 0x7E,
                "encoded output contains non-printable ASCII: 0x{b:02x}"
            );
        }
    }

    // ── Known test vectors ────────────────────────────────────

    #[test]
    fn encode_single_a() {
        let encoded = encode(b"A");
        let decoded = decode(&encoded);
        assert_eq!(decoded, b"A");
    }

    #[test]
    fn encode_all_printable_ascii() {
        let data: Vec<u8> = (0x20..=0x7E).collect();
        assert_eq!(decode(&encode(&data)), data);
    }

    // ── DECTAB validity ──────────────────────────────────────────

    #[test]
    fn dectab_has_exactly_91_valid_entries() {
        let valid_count = DECTAB.iter().filter(|&&v| v != INV).count();
        assert_eq!(valid_count, 91);
    }

    #[test]
    fn dectab_high_bytes_all_invalid() {
        for i in 128..=255usize {
            assert_eq!(
                DECTAB[i], INV,
                "DECTAB[{i}] should be INV for non-ASCII byte"
            );
        }
    }

    #[test]
    fn dectab_control_chars_all_invalid() {
        for i in 0..0x20usize {
            assert_eq!(DECTAB[i], INV, "DECTAB[{i}] should be INV for control char");
        }
        assert_eq!(DECTAB[0x7F], INV, "DECTAB[DEL] should be INV");
    }

    // ── Encoded output properties ────────────────────────────────

    #[test]
    fn non_empty_input_produces_non_empty_output() {
        for len in 1..=32 {
            let data = vec![0x42u8; len];
            let encoded = encode(&data);
            assert!(
                !encoded.is_empty(),
                "encode of {len} bytes should not be empty"
            );
        }
    }

    #[test]
    fn encode_output_length_monotonic() {
        let mut prev_len = 0;
        for size in [1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024] {
            let data: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();
            let enc_len = encode(&data).len();
            assert!(
                enc_len >= prev_len,
                "encoded length should be monotonically non-decreasing: {enc_len} < {prev_len}"
            );
            prev_len = enc_len;
        }
    }

    #[test]
    fn encoded_output_uses_only_enctab_chars() {
        let data: Vec<u8> = (0..1024).map(|i| (i * 41 % 256) as u8).collect();
        let encoded = encode(&data);
        let valid: std::collections::HashSet<u8> = ENCTAB.iter().copied().collect();
        for &b in &encoded {
            assert!(valid.contains(&b), "encoded byte 0x{b:02x} not in ENCTAB");
        }
    }

    // ── Edge-case roundtrips ─────────────────────────────────────

    #[test]
    fn roundtrip_single_null_byte() {
        assert_eq!(decode(&encode(&[0x00])), vec![0x00]);
    }

    #[test]
    fn roundtrip_single_0xff() {
        assert_eq!(decode(&encode(&[0xFF])), vec![0xFF]);
    }

    #[test]
    fn roundtrip_repeated_single_byte() {
        for &b in &[0x00, 0x42, 0x80, 0xFF] {
            let data = vec![b; 200];
            assert_eq!(
                decode(&encode(&data)),
                data,
                "roundtrip failed for 200x 0x{b:02x}"
            );
        }
    }

    #[test]
    fn roundtrip_utf8_text() {
        let text = "Hello 世界! 🌍 café résumé";
        let data = text.as_bytes();
        assert_eq!(decode(&encode(data)), data);
    }

    // ── Streaming with power-of-2 chunk sizes ────────────────────

    #[test]
    fn streaming_encode_various_chunk_sizes() {
        let data: Vec<u8> = (0..500).map(|i| (i * 13 % 256) as u8).collect();
        let bulk = encode(&data);
        for chunk_size in [1, 2, 4, 8, 16, 32, 64, 128, 256] {
            let mut result = Vec::new();
            {
                let mut encoder = Base91Encoder::new(&mut result);
                for chunk in data.chunks(chunk_size) {
                    encoder.write_all(chunk).unwrap();
                }
                encoder.flush().unwrap();
            }
            assert_eq!(
                result, bulk,
                "streaming encode with chunk_size={chunk_size} differs from bulk"
            );
        }
    }

    #[test]
    fn streaming_decode_various_chunk_sizes() {
        let data = b"streaming chunk decode verification";
        let encoded = encode(data);
        let bulk = decode(&encoded);
        for chunk_size in [1, 2, 3, 5, 7, 11] {
            let mut result = Vec::new();
            {
                let mut decoder = Base91Decoder::new(&mut result);
                for chunk in encoded.chunks(chunk_size) {
                    decoder.write_all(chunk).unwrap();
                }
                decoder.flush().unwrap();
            }
            assert_eq!(
                result, bulk,
                "streaming decode with chunk_size={chunk_size} differs from bulk"
            );
        }
    }

    // ── Encoder/decoder pipeline ─────────────────────────────────

    #[test]
    fn pipeline_encode_then_decode() {
        let original = b"pipeline roundtrip test data 12345";
        let mut encoded_buf = Vec::new();
        {
            let mut encoder = Base91Encoder::new(&mut encoded_buf);
            encoder.write_all(original).unwrap();
            encoder.flush().unwrap();
        }
        let mut decoded_buf = Vec::new();
        {
            let mut decoder = Base91Decoder::new(&mut decoded_buf);
            decoder.write_all(&encoded_buf).unwrap();
            decoder.flush().unwrap();
        }
        assert_eq!(decoded_buf, original);
    }

    // ── Empty write preserves state ──────────────────────────────

    #[test]
    fn empty_write_preserves_encoder_state() {
        let data = b"preserved";
        let expected = encode(data);
        let mut result = Vec::new();
        {
            let mut encoder = Base91Encoder::new(&mut result);
            encoder.write_all(&data[..4]).unwrap();
            encoder.write_all(b"").unwrap(); // empty write
            encoder.write_all(&data[4..]).unwrap();
            encoder.flush().unwrap();
        }
        assert_eq!(result, expected);
    }

    #[test]
    fn empty_write_preserves_decoder_state() {
        let encoded = encode(b"preserved decode");
        let expected = decode(&encoded);
        let mut result = Vec::new();
        {
            let mut decoder = Base91Decoder::new(&mut result);
            let mid = encoded.len() / 2;
            decoder.write_all(&encoded[..mid]).unwrap();
            decoder.write_all(b"").unwrap(); // empty write
            decoder.write_all(&encoded[mid..]).unwrap();
            decoder.flush().unwrap();
        }
        assert_eq!(result, expected);
    }

    // ── Capacity estimation ──────────────────────────────────────

    #[test]
    fn encode_capacity_never_exceeds_124_percent_for_bulk() {
        // For small inputs, overhead can be higher due to flush padding;
        // the 23% spec guarantee applies asymptotically
        for size in [100, 500, 2000, 10000] {
            let data: Vec<u8> = (0..size).map(|i| (i * 31 % 256) as u8).collect();
            let encoded = encode(&data);
            let ratio = encoded.len() as f64 / data.len() as f64;
            assert!(ratio <= 1.24, "encode ratio exceeds 1.24 for size {}", size);
        }
    }

    // ── Large data ───────────────────────────────────────────────

    #[test]
    fn roundtrip_100kb() {
        let data: Vec<u8> = (0..102400).map(|i| (i * 59 % 256) as u8).collect();
        assert_eq!(decode(&encode(&data)), data);
    }

    // ── Decoder with whitespace-wrapped encoded data ─────────────

    #[test]
    fn decode_with_crlf_line_wrapping() {
        let data = b"line-wrapped decode test";
        let encoded = encode(data);
        let encoded_str = String::from_utf8(encoded).unwrap();
        // Wrap every 10 chars with CRLF
        let wrapped: String = encoded_str
            .as_bytes()
            .chunks(10)
            .map(|chunk| std::str::from_utf8(chunk).unwrap())
            .collect::<Vec<_>>()
            .join("\r\n");
        assert_eq!(decode(wrapped.as_bytes()), data);
    }

    // ── Second-pass expansion ────────────────────────────────────

    #[test]
    fn decode_ignores_high_bytes_128_to_255() {
        let encoded = encode(b"high byte test");
        let mut with_junk = Vec::new();
        for &b in &encoded {
            with_junk.push(b);
            with_junk.push(0x80); // high byte, should be skipped
        }
        assert_eq!(decode(&with_junk), b"high byte test");
    }

    #[test]
    fn enctab_excludes_single_quote() {
        assert!(!ENCTAB.contains(&b'\''));
    }

    #[test]
    fn enctab_excludes_backslash() {
        assert!(!ENCTAB.contains(&b'\\'));
    }

    #[test]
    fn enctab_excludes_dash() {
        assert!(!ENCTAB.contains(&b'-'));
    }

    #[test]
    fn dectab_single_quote_is_invalid() {
        assert_eq!(DECTAB[b'\'' as usize], INV);
    }

    #[test]
    fn dectab_backslash_is_invalid() {
        assert_eq!(DECTAB[b'\\' as usize], INV);
    }

    #[test]
    fn roundtrip_power_of_two_boundaries() {
        for exp in 0..=12 {
            let len = 1usize << exp;
            let data: Vec<u8> = (0..len).map(|i| (i * 41 % 256) as u8).collect();
            assert_eq!(
                decode(&encode(&data)),
                data,
                "roundtrip failed at 2^{exp} = {len} bytes"
            );
        }
    }

    #[test]
    fn roundtrip_13_byte_boundary() {
        // 13 bits is a key threshold in the encoder
        for len in 12..=15 {
            let data: Vec<u8> = (0..len).map(|i| i as u8).collect();
            assert_eq!(decode(&encode(&data)), data);
        }
    }

    #[test]
    fn roundtrip_alternating_ff_00() {
        let data: Vec<u8> = (0..128)
            .map(|i| if i % 2 == 0 { 0xFF } else { 0x00 })
            .collect();
        assert_eq!(decode(&encode(&data)), data);
    }

    #[test]
    fn encode_concat_not_same_as_encode_whole() {
        let a = b"hello";
        let b = b"world";
        let mut combined = Vec::new();
        combined.extend_from_slice(a);
        combined.extend_from_slice(b);
        let enc_whole = encode(&combined);
        let mut enc_parts = encode(a);
        enc_parts.extend_from_slice(&encode(b));
        // Concatenating two independently encoded segments generally
        // differs from encoding the whole because of internal state
        assert_ne!(enc_parts, enc_whole);
        // But each segment still decodes correctly on its own
        assert_eq!(decode(&encode(a)), a.as_slice());
        assert_eq!(decode(&encode(b)), b.as_slice());
    }

    #[test]
    fn decode_single_valid_char_produces_output_after_flush() {
        // A single valid alphabet char should produce at least something when flushed
        let mut result = Vec::new();
        {
            let mut decoder = Base91Decoder::new(&mut result);
            // 'A' maps to DECTAB['A'] = 0, which is a valid entry
            decoder.write_all(b"A").unwrap();
            decoder.flush().unwrap();
        }
        // With only one valid char, the decoder has a pending value
        // Flush should emit something
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn decode_two_valid_chars_produces_output() {
        let mut result = Vec::new();
        {
            let mut decoder = Base91Decoder::new(&mut result);
            decoder.write_all(b"AB").unwrap();
            decoder.flush().unwrap();
        }
        assert!(!result.is_empty());
    }

    #[test]
    fn encoder_write_all_empty_no_output() {
        let mut result = Vec::new();
        {
            let mut encoder = Base91Encoder::new(&mut result);
            encoder.write_all(b"").unwrap();
            encoder.flush().unwrap();
        }
        assert!(result.is_empty());
    }

    #[test]
    fn decoder_write_all_empty_no_output() {
        let mut result = Vec::new();
        {
            let mut decoder = Base91Decoder::new(&mut result);
            decoder.write_all(b"").unwrap();
            decoder.flush().unwrap();
        }
        assert!(result.is_empty());
    }

    #[test]
    fn roundtrip_50kb_pseudo_random() {
        let mut data = Vec::with_capacity(50000);
        let mut state: u32 = 0xDEAD_BEEF;
        for _ in 0..50000 {
            // Simple LCG
            state = state.wrapping_mul(1103515245).wrapping_add(12345);
            data.push((state >> 16) as u8);
        }
        assert_eq!(decode(&encode(&data)), data);
    }

    #[test]
    fn encoded_length_grows_with_input() {
        let small = encode(&[0x42; 10]);
        let large = encode(&[0x42; 1000]);
        assert!(large.len() > small.len());
    }

    #[test]
    fn roundtrip_just_below_and_above_val_88_threshold() {
        // The encoder uses 88 as a threshold: val > 88 takes 13 bits, else 14 bits
        // Exercise data patterns that produce values near this boundary
        let data_low: Vec<u8> = vec![0x58; 50]; // values that tend to produce val <= 88
        let data_high: Vec<u8> = vec![0xFF; 50]; // values that tend to produce val > 88
        assert_eq!(decode(&encode(&data_low)), data_low);
        assert_eq!(decode(&encode(&data_high)), data_high);
    }

    #[test]
    fn encode_is_pure_function() {
        // encode() should not have side effects; calling it twice on
        // different data should not interfere
        let a = encode(b"first");
        let b = encode(b"second");
        let a2 = encode(b"first");
        assert_eq!(a, a2);
        assert_ne!(a, b);
    }

    #[test]
    fn decode_is_pure_function() {
        let enc = encode(b"pure decode test");
        let d1 = decode(&enc);
        let d2 = decode(&enc);
        assert_eq!(d1, d2);
    }

    #[test]
    fn enctab_first_26_are_uppercase() {
        for i in 0..26 {
            assert!(
                ENCTAB[i].is_ascii_uppercase(),
                "ENCTAB[{i}] = {} is not uppercase",
                ENCTAB[i] as char
            );
        }
    }

    #[test]
    fn enctab_next_26_are_lowercase() {
        for i in 26..52 {
            assert!(
                ENCTAB[i].is_ascii_lowercase(),
                "ENCTAB[{i}] = {} is not lowercase",
                ENCTAB[i] as char
            );
        }
    }

    // ── Third-pass expansion ────────────────────────────────────

    #[test]
    fn enctab_indices_52_to_61_are_digits() {
        for i in 52..62 {
            assert!(
                ENCTAB[i].is_ascii_digit(),
                "ENCTAB[{i}] = {} is not a digit",
                ENCTAB[i] as char
            );
        }
    }

    #[test]
    fn enctab_last_byte_is_double_quote() {
        assert_eq!(ENCTAB[90], b'"');
    }

    #[test]
    fn dectab_maps_A_to_zero() {
        assert_eq!(DECTAB[b'A' as usize], 0);
    }

    #[test]
    fn dectab_maps_a_to_26() {
        assert_eq!(DECTAB[b'a' as usize], 26);
    }

    #[test]
    fn dectab_maps_0_to_52() {
        assert_eq!(DECTAB[b'0' as usize], 52);
    }

    #[test]
    fn roundtrip_enctab_itself() {
        // ENCTAB is valid binary data; ensure it roundtrips
        assert_eq!(decode(&encode(&ENCTAB)), ENCTAB.to_vec());
    }

    #[test]
    fn zero_data_encodes_shorter_than_random_data() {
        let zeros = vec![0u8; 500];
        let random: Vec<u8> = (0..500).map(|i| (i * 197 % 256) as u8).collect();
        let enc_zeros = encode(&zeros);
        let enc_random = encode(&random);
        assert!(
            enc_zeros.len() < enc_random.len(),
            "zero data ({}) should encode shorter than random data ({})",
            enc_zeros.len(),
            enc_random.len()
        );
    }

    #[test]
    fn roundtrip_bytes_near_val_88_boundary() {
        // Test specific byte values around internal threshold
        for b in 85..=92 {
            let data = vec![b as u8; 20];
            assert_eq!(
                decode(&encode(&data)),
                data,
                "roundtrip failed for 20x byte {b}"
            );
        }
    }

    #[test]
    fn decode_mixed_valid_and_invalid_interleaved() {
        let data = b"interleaved";
        let encoded = encode(data);
        let mut mixed = Vec::new();
        for &b in &encoded {
            mixed.push(0x01); // invalid (control char)
            mixed.push(b);
            mixed.push(0x7F); // DEL, invalid
        }
        assert_eq!(decode(&mixed), data);
    }

    #[test]
    fn flush_encode_more_flush_again() {
        let mut result = Vec::new();
        {
            let mut encoder = Base91Encoder::new(&mut result);
            encoder.write_all(b"part1").unwrap();
            encoder.flush().unwrap();
            encoder.write_all(b"part2").unwrap();
            encoder.flush().unwrap();
        }
        // The flushed output should decode to concatenated parts
        // (but note: each flush completes a standalone encoding segment)
        // We can't simply decode the whole result to get "part1part2"
        // because the internal state is reset between flushes.
        // Instead, verify that both parts encoded something.
        assert!(!result.is_empty());
    }

    #[test]
    fn roundtrip_all_byte_pairs() {
        // Test all 256 possible two-byte pairs with first byte = second byte
        for b in 0..=255u8 {
            let data = vec![b, b];
            assert_eq!(
                decode(&encode(&data)),
                data,
                "roundtrip failed for pair [{b}, {b}]"
            );
        }
    }

    #[test]
    fn encode_write_returns_zero_for_empty_slice() {
        let mut result = Vec::new();
        let mut encoder = Base91Encoder::new(&mut result);
        assert_eq!(encoder.write(b"").unwrap(), 0);
    }

    #[test]
    fn decode_write_returns_zero_for_empty_slice() {
        let mut result = Vec::new();
        let mut decoder = Base91Decoder::new(&mut result);
        assert_eq!(decoder.write(b"").unwrap(), 0);
    }

    #[test]
    fn roundtrip_seven_bytes() {
        // 7 bytes exercises flush with 7 leftover bits
        let data = b"SevenCh";
        assert_eq!(decode(&encode(data)), data);
    }

    #[test]
    fn roundtrip_eight_bytes() {
        let data = b"EightChs";
        assert_eq!(decode(&encode(data)), data);
    }

    #[test]
    fn encode_produces_at_most_two_bytes_per_input_byte() {
        // For any input, encoded length should be at most ~1.23x + small constant
        for len in 1..=50 {
            let data = vec![0xAA; len];
            let encoded = encode(&data);
            assert!(
                encoded.len() <= len * 2 + 2,
                "encoded {} bytes into {} bytes (input len {})",
                len,
                encoded.len(),
                len
            );
        }
    }

    #[test]
    fn dectab_double_quote_maps_to_90() {
        assert_eq!(DECTAB[b'"' as usize], 90);
    }

    #[test]
    fn enctab_no_double_encoding_needed() {
        // Every byte in ENCTAB should be a printable ASCII character
        // that doesn't need escaping in most contexts
        for &b in &ENCTAB {
            assert!(
                b >= 0x20 && b <= 0x7E,
                "ENCTAB has non-printable: 0x{b:02x}"
            );
        }
    }
}
