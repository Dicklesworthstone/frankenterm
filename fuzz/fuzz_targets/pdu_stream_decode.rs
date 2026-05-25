#![no_main]

use codec::Pdu;
use libfuzzer_sys::fuzz_target;

const MAX_INPUT_LEN: usize = 64 * 1024;
const MAX_DECODE_STEPS: usize = 8;
const MAX_PDU_SIZE: u64 = 256 * 1024 * 1024;
const COMPRESSED_MASK: u64 = 1 << 63;
const MAX_SYNTHETIC_BODY: usize = 64;

fn leb128_u64(mut value: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(10);
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
    out
}

fn u64_from_prefix(bytes: &[u8]) -> u64 {
    let mut value = 0u64;
    for (offset, byte) in bytes.iter().copied().take(8).enumerate() {
        value |= u64::from(byte) << (offset * 8);
    }
    value
}

fn synthesized_boundary_frame(data: &[u8]) -> Option<(Vec<u8>, bool)> {
    let (&selector, rest) = data.split_first()?;
    let raw_len = match selector % 8 {
        0 => 0,
        1 => 1,
        2 => 2,
        3 => MAX_PDU_SIZE.saturating_sub(1),
        4 => MAX_PDU_SIZE,
        5 => MAX_PDU_SIZE + 1,
        6 => COMPRESSED_MASK - 1,
        _ => u64::from(selector & 0x1f) + 2,
    };
    let tagged_len = if selector & 0x80 != 0 {
        raw_len | COMPRESSED_MASK
    } else {
        raw_len
    };
    let serial = u64_from_prefix(rest);
    let ident = u64_from_prefix(rest.get(8..).unwrap_or_default());
    let body = rest.get(16..).unwrap_or_default();

    let mut frame = leb128_u64(tagged_len);
    frame.extend(leb128_u64(serial));
    frame.extend(leb128_u64(ident));
    frame.extend_from_slice(&body[..body.len().min(MAX_SYNTHETIC_BODY)]);

    Some((frame, raw_len > MAX_PDU_SIZE))
}

fn exercise_synthesized_boundary_frame(data: &[u8]) {
    let Some((frame, should_reject_oversize)) = synthesized_boundary_frame(data) else {
        return;
    };

    let direct = Pdu::decode(frame.as_slice());
    let mut stream_buffer = frame.clone();
    let stream = Pdu::stream_decode(&mut stream_buffer);

    if should_reject_oversize {
        assert!(
            direct.is_err(),
            "direct decode accepted oversized synthetic PDU frame"
        );
        assert!(
            stream.is_err(),
            "stream_decode accepted oversized synthetic PDU frame"
        );
        assert_eq!(
            stream_buffer, frame,
            "oversized stream frame should be rejected before consuming bytes"
        );
        return;
    }

    if let Ok(Some(streamed)) = stream {
        let direct = direct.expect("stream_decode accepted frame that direct decode rejected");
        assert_eq!(
            direct.serial, streamed.serial,
            "synthetic stream/direct serial mismatch"
        );
        assert_eq!(
            direct.pdu, streamed.pdu,
            "synthetic stream/direct PDU mismatch"
        );
    }
}

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_LEN {
        return;
    }

    exercise_synthesized_boundary_frame(data);

    let mut buffer = data.to_vec();

    for _ in 0..MAX_DECODE_STEPS {
        let before = buffer.clone();
        let before_len = buffer.len();

        match Pdu::stream_decode(&mut buffer) {
            Ok(Some(decoded)) => {
                let _ = decoded.serial;
                let _ = decoded.pdu.pdu_name();

                let consumed_len = before_len.saturating_sub(buffer.len());
                let one_frame = &before[..consumed_len];
                let direct = Pdu::decode(one_frame)
                    .expect("stream_decode accepted a complete frame that direct decode rejected");
                assert_eq!(
                    direct.serial, decoded.serial,
                    "stream_decode and direct decode disagree on serial"
                );
                assert_eq!(
                    direct.pdu, decoded.pdu,
                    "stream_decode and direct decode disagree on PDU"
                );
            }
            Ok(None) | Err(_) => break,
        }

        if buffer.len() >= before_len {
            break;
        }
    }
});
