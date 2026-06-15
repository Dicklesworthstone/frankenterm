//! ft-zeo5o: `ft web` builds a publisher-less `EventBus`, so `/stream/events`
//! emits only the `ready` frame + keepalives when run standalone.
//!
//! Reality (reality-check sweep, verified): the `Commands::Web` arm constructs a
//! fresh `EventBus` with no publisher and spawns no watcher/capture/detection
//! task, and an in-memory `EventBus` cannot be shared across processes — so a
//! separate `ft watch` does not feed this server. The SSE handler is correct; the
//! defect is the missing in-process producer plus docs advertising "live
//! EventBus" streaming that does not happen standalone.
//!
//! These guards lock in the honest `web.rs` module doc. **If you wire a producer
//! (an in-process capture pipeline or a storage-tail -> `EventBus` bridge), update
//! `web.rs`'s module doc AND these guards together.**

use std::path::PathBuf;

fn web_src() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("web.rs");
    std::fs::read_to_string(&path).expect("read web.rs")
}

#[test]
fn web_module_doc_states_stream_events_has_no_standalone_publisher() {
    let src = web_src();
    assert!(
        src.contains("carries live events only when an in-process producer"),
        "web.rs module doc must state that /stream/events carries live events only with an \
         in-process producer — standalone `ft web` has none (ft-zeo5o)"
    );
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
