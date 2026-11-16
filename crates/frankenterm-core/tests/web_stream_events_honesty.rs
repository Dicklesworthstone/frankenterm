//! ft-zeo5o: standalone `ft web` tails persisted detection events onto its bus.
//!
//! The storage bridge supplies events written by a separate watcher; an
//! in-memory EventBus itself cannot span processes. These source/documentation
//! guards preserve the bridge wiring and its explicit in-process opt-out.
//!
//! Transport and delivery behavior still requires the executable web tests.

use std::path::PathBuf;

fn web_src() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("web.rs");
    std::fs::read_to_string(&path).expect("read web.rs")
}

#[test]
fn web_module_doc_and_source_preserve_the_storage_event_bridge() {
    let src = web_src();
    assert!(
        src.contains("server tails newly persisted")
            && src.contains("server::spawn_storage_event_tail")
            && src.contains("with_storage_event_tail(false)"),
        "web.rs must document persisted-event delivery and the in-process producer opt-out"
    );
    assert!(src.contains("storage_event_tail: true"));
    let server = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/web/server.rs"),
    )
    .expect("read web/server.rs");
    assert!(server.contains("if config.storage_event_tail_enabled()"));
    assert!(server.contains("spawn_storage_event_tail("));
    assert!(server.contains("storage.get_events_stream(query).await"));
    assert!(server.contains("bus.publish(stored_event_to_bus_event(stored))"));
}

#[test]
fn web_module_doc_dropped_the_blanket_live_streams_claim() {
    let src = web_src();
    assert!(
        !src.contains("plus SSE subscriptions for live event/delta streams"),
        "the misleading 'SSE subscriptions for live event/delta streams' claim must stay \
         removed until /stream/events has a production publisher (ft-zeo5o)"
    );
}
