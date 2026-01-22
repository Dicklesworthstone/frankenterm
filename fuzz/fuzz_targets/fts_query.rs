#![no_main]

use libfuzzer_sys::fuzz_target;
use rusqlite::{Connection, params};
use wa_core::storage::initialize_schema;

fuzz_target!(|data: &[u8]| {
    if data.len() > 8_192 {
        return;
    }

    let query = match std::str::from_utf8(data) {
        Ok(text) => text,
        Err(_) => return,
    };

    let conn = match Connection::open_in_memory() {
        Ok(conn) => conn,
        Err(_) => return,
    };

    if initialize_schema(&conn).is_err() {
        return;
    }

    let _ = conn.execute(
        "INSERT INTO panes (pane_id, first_seen_at, last_seen_at) VALUES (1, 0, 0)",
        [],
    );

    let _ = conn.execute(
        "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (1, 0, 'seed', 4, 0)",
        [],
    );

    let _ = conn.query_row(
        "SELECT COUNT(*) FROM output_segments_fts WHERE output_segments_fts MATCH ?1 LIMIT 1",
        params![query],
        |_| Ok::<_, rusqlite::Error>(()),
    );
});
