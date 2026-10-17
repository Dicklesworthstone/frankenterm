//! Zero allocations per durable scrollback row in steady state
//! (ft-yccm0.2.1.4 AC1).
//!
//! The store's batches serialize, compress and seal their compact rows
//! through a per-thread encode arena. Once a window has grown it to fit,
//! a window of rows in the storage Screen hands over (clustered) allocates
//! nothing. jemalloc's per-thread cumulative counter (`thread.allocatedp`)
//! measures this thread exactly: any allocation, however small, raises it.
//! frankenterm-alloc installs jemalloc as the global allocator for this
//! crate's tests (see Cargo.toml).
#![cfg(not(windows))]

use frankenterm_mux_server_impl::scrollback_record_bench::{Rows, Sealer};
use tikv_jemalloc_ctl::thread;

/// This thread's cumulative allocated bytes. The pointer is looked up once;
/// each read is a plain load, so measuring does not itself call jemalloc.
#[derive(Clone, Copy)]
struct Counter(thread::ThreadLocal<u64>);

impl Counter {
    fn new() -> Self {
        Counter(
            thread::allocatedp::mib()
                .and_then(|mib| mib.read())
                .expect("jemalloc thread.allocatedp"),
        )
    }

    fn bytes(self) -> u64 {
        self.0.get()
    }
}

#[test]
fn steady_state_windows_encode_rows_without_allocating() {
    assert_eq!(
        frankenterm_alloc::allocator_backend(),
        frankenterm_alloc::AllocatorBackend::Jemalloc,
        "the counter only sees jemalloc's allocations"
    );
    let sealer = Sealer::new();
    let mut stream = sealer.compact_stream();
    let counter = Counter::new();
    // The counter is live: a zero below is a measurement, not a dead read.
    let before = counter.bytes();
    let probe = std::hint::black_box(vec![0_u8; 4096]);
    assert!(
        counter.bytes() - before >= 4096,
        "the counter sees this thread's allocations"
    );
    drop(probe);
    for (shape, rows) in [
        ("seq-like ascii9", Rows::printable(512, 9)),
        ("ascii91", Rows::printable(512, 91)),
        ("T0-like emoji colors", Rows::emoji_colors(256, 80)),
    ] {
        let rows = rows.compressed_for_scrollback();
        assert!(
            rows.all_clustered(),
            "{shape}: rows reach the store clustered"
        );
        // The first window grows the arena to fit these rows.
        let warm_bytes = sealer.seal_compact_arena(&rows, &mut stream);
        let before = counter.bytes();
        let window_bytes = sealer.seal_compact_arena(&rows, &mut stream);
        let allocated = counter.bytes() - before;
        assert_eq!(
            window_bytes, warm_bytes,
            "{shape}: the same rows seal to the same bytes"
        );
        assert_eq!(
            allocated, 0,
            "{shape}: a steady-state window of {window_bytes} record bytes allocated {allocated} bytes"
        );
    }
}
