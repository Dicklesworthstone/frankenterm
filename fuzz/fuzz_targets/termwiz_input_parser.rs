#![no_main]

use libfuzzer_sys::fuzz_target;
use termwiz::input::{InputEvent, InputParser};

const MAX_INPUT_BYTES: usize = 64 * 1024;
const CHUNK_SCHEDULE: &[usize] = &[1, 2, 3, 5, 8, 13, 21, 34];

fn parse_as_vec(data: &[u8], maybe_more: bool) -> Vec<InputEvent> {
    let mut parser = InputParser::new();
    parser.parse_as_vec(data, maybe_more)
}

fn parse_callback(data: &[u8], maybe_more: bool) -> Vec<InputEvent> {
    let mut parser = InputParser::new();
    let mut events = Vec::new();
    parser.parse(data, |event| events.push(event), maybe_more);
    events
}

fn parse_then_flush(data: &[u8]) -> Vec<InputEvent> {
    let mut parser = InputParser::new();
    let mut events = Vec::new();
    parser.parse(data, |event| events.push(event), true);
    parser.parse(b"", |event| events.push(event), false);
    events
}

fn parse_chunked_then_flush(data: &[u8]) -> Vec<InputEvent> {
    let mut parser = InputParser::new();
    let mut events = Vec::new();
    let mut offset = 0;
    let mut schedule_idx = 0;

    while offset < data.len() {
        let chunk_len =
            CHUNK_SCHEDULE[schedule_idx % CHUNK_SCHEDULE.len()].min(data.len() - offset);
        parser.parse(
            &data[offset..offset + chunk_len],
            |event| events.push(event),
            true,
        );
        offset += chunk_len;
        schedule_idx += 1;
    }

    parser.parse(b"", |event| events.push(event), false);
    events
}

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }

    let one_shot = parse_as_vec(data, false);
    let callback = parse_callback(data, false);
    assert_eq!(
        callback,
        one_shot,
        "InputParser::parse callback events diverged from parse_as_vec events \
         for {} input bytes",
        data.len()
    );

    let deferred = parse_then_flush(data);
    assert_eq!(
        deferred,
        one_shot,
        "InputParser maybe_more=true followed by an explicit no-more flush \
         diverged from one-shot no-more parsing for {} input bytes",
        data.len()
    );

    let chunked = parse_chunked_then_flush(data);
    assert_eq!(
        chunked,
        one_shot,
        "InputParser chunked maybe_more parsing diverged from one-shot no-more \
         parsing for {} input bytes",
        data.len()
    );
});
