#![no_main]

use libfuzzer_sys::fuzz_target;
use termwiz::input::InputParser;

const MAX_INPUT_BYTES: usize = 1_000_000;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }

    let mut parser = InputParser::new();
    let _ = parser.parse_as_vec(data, false);

    let mut parser = InputParser::new();
    let _ = parser.parse_as_vec(data, true);
    let _ = parser.parse_as_vec(b"", false);
});
