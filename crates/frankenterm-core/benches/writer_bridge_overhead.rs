//! `with_writer_backend` overhead micro-bench (br-ft-7yq2z).
//!
//! **Bead:** `ft-7yq2z` — follow-up to `ft-l1jgo` writer-thread
//! migration substrate landed at bab94d08d. The slice introduced
//! `with_writer_backend(conn, |backend| { ... })` in storage.rs
//! that does this per call:
//!
//! ```ignore
//! let placeholder = Connection::open_in_memory().expect(...);
//! let owned = std::mem::replace(conn, placeholder);
//! let backend = RusqliteBackend::new(owned);
//! let result = f(&backend);
//! *conn = backend.into_connection();
//! result
//! ```
//!
//! `Connection::open_in_memory()` is the suspected hot spot
//! (~200 µs on a warm SQLite, varies with libsqlite3-sys init).
//! As ft-l1jgo continues and ~30 more `_sync` writer helpers
//! migrate to the bridge, that cost compounds on every
//! WriteCommand dispatch — including the high-volume
//! AppendSegment path. The bead wants a measured number so
//! the optimization decision (cache the placeholder, restructure
//! writer_loop to wrap-once, or leave alone) can be data-driven.
//!
//! ## What this bench measures
//!
//! Three legs, all writing into the same in-memory schema:
//!
//! 1. **`baseline_direct_execute`** — `conn.execute("UPDATE ...",
//!    params![...])` on a long-lived `Connection`. This is the
//!    pre-migration writer-thread path: every `_sync` helper
//!    typed `conn: &Connection` and called `conn.execute(...)`
//!    directly.
//! 2. **`bridge_with_writer_backend_dance`** — the full
//!    wrap/unwrap dance the migration introduces, exactly as
//!    storage.rs::with_writer_backend ships it. Includes the
//!    placeholder allocation.
//! 3. **`bridge_without_placeholder`** — same dance but reusing
//!    a pre-built placeholder Connection (mimics the
//!    optimization candidate (1) "cache the placeholder once
//!    via OnceLock"). Isolates the placeholder cost from the
//!    rest of the dance overhead.
//!
//! ## Reading the output
//!
//! The bridge tax is `bridge_with_writer_backend_dance −
//! baseline_direct_execute`. The optimization headroom is
//! `bridge_with_writer_backend_dance −
//! bridge_without_placeholder`. If the headroom is most of
//! the tax, the cache-the-placeholder optimization is
//! worth landing; if not, the bigger restructure (option 2 in
//! the bead's fix shape — wrap conn ONCE at the top of
//! writer_loop) is the real win.
//!
//! ## Workload notes
//!
//! - Schema: a single `mux_sessions(session_id TEXT, shutdown_clean INTEGER)`
//!   table, mirroring the actual workload of the first
//!   `_sync → _backend` migration (`mark_session_shutdown_clean`).
//! - Each iteration runs ONE UPDATE to bound noise from query
//!   complexity. The wrap/unwrap overhead, not the SQL itself,
//!   is the measurement target.
//! - `rusqlite_memory` connections — keeps everything in RAM
//!   so disk fsync doesn't dominate the per-iter cost.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use frankenterm_core::storage_backend_helpers::execute_typed;
use frankenterm_core::storage_backend_trait::{RusqliteBackend, ToSqlValue};
use rusqlite::{Connection, params};

const SCHEMA: &str = "CREATE TABLE mux_sessions (session_id TEXT NOT NULL UNIQUE, \
                      shutdown_clean INTEGER NOT NULL DEFAULT 0);";

const SEED_SQL: &str =
    "INSERT INTO mux_sessions (session_id, shutdown_clean) VALUES ('s-bench', 0);";

const UPDATE_SQL: &str = "UPDATE mux_sessions SET shutdown_clean = 1 WHERE session_id = ?1";

const SESSION_ID: &str = "s-bench";

fn fresh_seeded_conn() -> Connection {
    let conn = Connection::open_in_memory().expect("open in-memory bench connection");
    conn.execute_batch(SCHEMA).expect("create schema");
    conn.execute_batch(SEED_SQL).expect("seed row");
    conn
}

/// Baseline: pre-migration writer-thread path. Long-lived
/// `Connection` + direct `conn.execute(...)` per call.
fn bench_baseline_direct_execute(c: &mut Criterion) {
    let conn = fresh_seeded_conn();
    c.bench_function("writer_bridge/baseline_direct_execute", |b| {
        b.iter(|| {
            let n = conn
                .execute(UPDATE_SQL, params![SESSION_ID])
                .expect("direct execute");
            black_box(n);
        });
    });
}

/// Migration path: full wrap/unwrap dance with per-call
/// placeholder allocation, mirroring storage.rs::with_writer_backend.
fn bench_bridge_with_placeholder_alloc(c: &mut Criterion) {
    let mut conn = fresh_seeded_conn();
    c.bench_function("writer_bridge/with_writer_backend_dance", |b| {
        b.iter(|| {
            // The dance — exactly as storage.rs::with_writer_backend
            // ships it (sans the closure indirection so the
            // benched cost is the wrap mechanics, not the call
            // site's lambda overhead).
            let placeholder =
                Connection::open_in_memory().expect("placeholder Connection for bridge dance");
            let owned = std::mem::replace(&mut conn, placeholder);
            let backend = RusqliteBackend::new(owned);
            execute_typed(&backend, UPDATE_SQL, &[ToSqlValue::Text(SESSION_ID)])
                .expect("backend execute_typed");
            conn = backend.into_connection();
            black_box(&conn);
        });
    });
}

/// Optimization candidate (1) from the bead: cache the
/// placeholder once and reuse it across calls. Isolates the
/// placeholder allocation cost from the rest of the dance.
///
/// Pattern: borrow the cached placeholder out of its `Option`
/// slot for the swap, recover it from the conn slot via a
/// second `mem::replace` after the backend hands the live
/// conn back, and put it back into the cache. Net allocation
/// across the entire benchmark loop is one (the initial
/// `cached_placeholder = Some(open_in_memory())`).
fn bench_bridge_without_placeholder_alloc(c: &mut Criterion) {
    let mut conn = fresh_seeded_conn();
    let mut cached_placeholder =
        Some(Connection::open_in_memory().expect("cache the placeholder once"));
    c.bench_function("writer_bridge/with_writer_backend_dance_cached", |b| {
        b.iter(|| {
            // Borrow the cached placeholder out of its slot.
            let placeholder = cached_placeholder
                .take()
                .expect("placeholder always available between iterations");
            // Wrap the live conn into a backend. The placeholder
            // sits in the conn slot for the duration of f().
            let owned = std::mem::replace(&mut conn, placeholder);
            let backend = RusqliteBackend::new(owned);
            execute_typed(&backend, UPDATE_SQL, &[ToSqlValue::Text(SESSION_ID)])
                .expect("backend execute_typed");
            // Pull the live conn back out and recover the
            // placeholder from the slot in a single swap. Now
            // the placeholder is in `recovered_placeholder` and
            // can go back into the cache for the next iter.
            let recovered_placeholder = std::mem::replace(&mut conn, backend.into_connection());
            cached_placeholder = Some(recovered_placeholder);
            black_box(&conn);
        });
    });
}

/// Just the placeholder allocation in isolation — the headline
/// number the bead's fix-shape paragraph references ("~200 µs").
fn bench_just_placeholder_alloc(c: &mut Criterion) {
    c.bench_function("writer_bridge/connection_open_in_memory_only", |b| {
        b.iter(|| {
            let conn = Connection::open_in_memory().expect("placeholder alloc");
            black_box(&conn);
            // Drop closes the connection; that drop cost is part
            // of the per-iter envelope on the dance bench too.
        });
    });
}

criterion_group!(
    benches,
    bench_just_placeholder_alloc,
    bench_baseline_direct_execute,
    bench_bridge_with_placeholder_alloc,
    bench_bridge_without_placeholder_alloc,
);
criterion_main!(benches);
