#![no_main]

use codec::Pdu;
use libfuzzer_sys::fuzz_target;

const MAX_INPUT_LEN: usize = 64 * 1024;
const MAX_DECODE_STEPS: usize = 8;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_LEN {
        return;
    }

    let mut buffer = data.to_vec();

    for _ in 0..MAX_DECODE_STEPS {
        let before_len = buffer.len();

        match Pdu::stream_decode(&mut buffer) {
            Ok(Some(decoded)) => {
                let _ = decoded.serial;
                let _ = decoded.pdu.pdu_name();
            }
            Ok(None) | Err(_) => break,
        }

        if buffer.len() >= before_len {
            break;
        }
    }
});
