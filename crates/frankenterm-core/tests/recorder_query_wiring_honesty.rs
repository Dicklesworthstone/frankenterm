//! ft-ja9i1: lock in that the `RecorderQueryExecutor`
//! authorize -> execute -> redact -> audit gate is documented as NOT wired to
//! any production read path.
//!
//! Reality (reality-check sweep, verified): no CLI / robot-mode / MCP / runtime
//! surface routes recorder reads through `RecorderQueryExecutor::execute`; the
//! executor, its elevation grants (`grant_elevation` / `revoke_elevation`), and
//! the elevation-TTL sweep (`expire_grants`) are reachable only from tests; and
//! the executor's `RecorderEventReader` has a single impl — the in-memory test
//! store. So the documented access-control + redaction + audit-hash-chain read
//! gate enforces nothing in production.
//!
//! A module doc that claims the gate IS enforced ("Any interface surface (CLI,
//! robot mode, MCP) calls this module") is a security-relevant overpromise:
//! readers would assume recorder reads are access-controlled when they are not.
//! These guards pin the honest wording. **If you wire the gate to a real
//! production read path, update `recorder_query.rs`'s module doc AND these
//! guards together.**

use std::path::PathBuf;

fn recorder_query_src() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("recorder_query.rs");
    std::fs::read_to_string(&path).expect("read recorder_query.rs")
}

#[test]
fn recorder_query_module_doc_admits_it_is_not_production_wired() {
    let src = recorder_query_src();
    assert!(
        src.contains("This gate is NOT enforced on any production read path"),
        "recorder_query.rs module doc must carry the ft-ja9i1 wiring-status warning: \
         the authz->redaction->audit gate has no production caller and must not be \
         documented as an enforced guarantee"
    );
}

#[test]
fn recorder_query_module_doc_dropped_the_false_any_interface_claim() {
    let src = recorder_query_src();
    assert!(
        !src.contains("Any interface surface (CLI, robot mode, MCP) calls this module"),
        "the false 'any interface surface calls this module' claim must stay removed \
         until the gate is actually wired to a production read path (ft-ja9i1)"
    );
}
