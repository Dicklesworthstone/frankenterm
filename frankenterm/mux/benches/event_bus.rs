//! Criterion benchmarks for the native EventBus dispatch path.
//!
//! Measures native handler dispatch overhead to verify the <1 μs target
//! required by wa-3dfxb.13.

use anyhow::Error;
use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use frankenterm_term::color::ColorPalette;
use frankenterm_term::{Terminal, TerminalConfiguration, TerminalSize};
use mux::domain::DomainId;
use mux::events::{
    Event, EventAction, EventBus, EventClockDomain, EventPayload, EventTimestamp, EventType,
    HandlerFn, HandlerPriority,
};
use mux::localpane::LocalPane;
use mux::pane::{Pane, PaneId};
use portable_pty::{Child, ChildKiller, ExitStatus, MasterPty, PtySize};
use std::hint::black_box;
use std::io::{Cursor, Read, Result as IoResult, Write};
use std::sync::Arc;
use uuid::Uuid;

// Keep below LocalPane's disruptor ring capacity (1024). The staged-contention
// bench intentionally holds the terminal lock, so filling the ring would enter
// the production blocking fallback and deadlock the harness.
const PANE_IO_BATCH_COUNT: usize = 256;

fn bench_timestamp(monotonic_ns: u64) -> EventTimestamp {
    EventTimestamp::from_parts(
        EventClockDomain::new(Uuid::from_u128(1), 1).expect("benchmark clock domain is non-nil"),
        monotonic_ns,
        None,
    )
}

/// Benchmark: fire a single native handler (no filter).
fn bench_fire_single_native(c: &mut Criterion) {
    let bus = EventBus::new();
    let handler: Arc<HandlerFn> = Arc::new(|_| vec![]);
    bus.register(HandlerPriority::Native, None, handler)
        .expect("handler registration should succeed");

    let event = Event::with_timestamp(
        EventType::PaneOutput,
        EventPayload::Empty,
        bench_timestamp(0),
    );

    c.bench_function("fire_single_native", |b| {
        b.iter(|| bus.fire(black_box(&event)));
    });
}

/// Benchmark: fire with 10 native handlers producing 1 action each.
fn bench_fire_10_native_handlers(c: &mut Criterion) {
    let bus = EventBus::new();
    for _ in 0..10 {
        let handler: Arc<HandlerFn> = Arc::new(|_| {
            vec![EventAction::Log {
                message: String::new(),
            }]
        });
        bus.register(HandlerPriority::Native, None, handler)
            .expect("handler registration should succeed");
    }

    let event = Event::with_timestamp(
        EventType::PaneOutput,
        EventPayload::Empty,
        bench_timestamp(0),
    );

    c.bench_function("fire_10_native_handlers", |b| {
        b.iter(|| bus.fire(black_box(&event)));
    });
}

/// Benchmark: fire with mixed priorities (3 native, 3 wasm, 3 lua).
fn bench_fire_mixed_priorities(c: &mut Criterion) {
    let bus = EventBus::new();
    let handler: Arc<HandlerFn> = Arc::new(|_| vec![]);

    for _ in 0..3 {
        bus.register(HandlerPriority::Native, None, handler.clone())
            .expect("handler registration should succeed");
    }
    for _ in 0..3 {
        bus.register(HandlerPriority::Wasm, None, handler.clone())
            .expect("handler registration should succeed");
    }
    for _ in 0..3 {
        bus.register(HandlerPriority::Lua, None, handler.clone())
            .expect("handler registration should succeed");
    }

    let event = Event::with_timestamp(
        EventType::PaneOutput,
        EventPayload::Empty,
        bench_timestamp(0),
    );

    c.bench_function("fire_mixed_9_handlers", |b| {
        b.iter(|| bus.fire(black_box(&event)));
    });
}

/// Benchmark: fire with event filter — only 1 of 10 handlers matches.
fn bench_fire_filtered(c: &mut Criterion) {
    let bus = EventBus::new();
    let handler: Arc<HandlerFn> = Arc::new(|_| vec![]);

    // 9 handlers for different event types.
    for _ in 0..9 {
        bus.register(
            HandlerPriority::Native,
            Some(EventType::UpdateStatus),
            handler.clone(),
        )
        .expect("handler registration should succeed");
    }
    // 1 handler for the type we'll fire.
    bus.register(
        HandlerPriority::Native,
        Some(EventType::PaneOutput),
        handler,
    )
    .expect("handler registration should succeed");

    let event = Event::with_timestamp(
        EventType::PaneOutput,
        EventPayload::Empty,
        bench_timestamp(0),
    );

    c.bench_function("fire_1_of_10_filtered", |b| {
        b.iter(|| bus.fire(black_box(&event)));
    });
}

/// Benchmark: register + deregister cycle.
fn bench_register_deregister(c: &mut Criterion) {
    let bus = EventBus::new();
    let handler: Arc<HandlerFn> = Arc::new(|_| vec![]);

    c.bench_function("register_deregister_cycle", |b| {
        b.iter(|| {
            let id = bus
                .register(HandlerPriority::Native, None, handler.clone())
                .expect("handler registration should succeed");
            bus.deregister(black_box(id));
        });
    });
}

/// Benchmark: fire with PaneText payload (simulates hot-path pane output).
fn bench_fire_pane_text_payload(c: &mut Criterion) {
    let bus = EventBus::new();
    let handler: Arc<HandlerFn> = Arc::new(|event| {
        if let EventPayload::PaneText { pane_id, .. } = &event.payload {
            vec![EventAction::Log {
                message: format!("pane {pane_id}"),
            }]
        } else {
            vec![]
        }
    });
    bus.register(
        HandlerPriority::Native,
        Some(EventType::PaneOutput),
        handler,
    )
    .expect("handler registration should succeed");

    let text: Arc<str> = Arc::from("$ cargo build\n   Compiling mux v0.1.0\n");
    let event = Event::with_timestamp(
        EventType::PaneOutput,
        EventPayload::PaneText {
            pane_id: 42,
            text: text.clone(),
        },
        bench_timestamp(0),
    );

    c.bench_function("fire_pane_text_payload", |b| {
        b.iter(|| bus.fire(black_box(&event)));
    });
}

/// Benchmark: 60 Hz update-status simulation (fire at render frequency).
fn bench_update_status_60hz(c: &mut Criterion) {
    let bus = EventBus::new();
    // Simulate 3 native status handlers.
    for _ in 0..3 {
        let handler: Arc<HandlerFn> = Arc::new(|_| vec![]);
        bus.register(
            HandlerPriority::Native,
            Some(EventType::UpdateStatus),
            handler,
        )
        .expect("handler registration should succeed");
    }

    let event = Event::with_timestamp(
        EventType::UpdateStatus,
        EventPayload::Status { pane_id: 0 },
        bench_timestamp(0),
    );

    c.bench_function("update_status_3_handlers", |b| {
        b.iter(|| bus.fire(black_box(&event)));
    });
}

#[derive(Debug)]
struct BenchTermConfig;

impl TerminalConfiguration for BenchTermConfig {
    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }
}

#[derive(Debug, Clone)]
struct BenchChild;

impl ChildKiller for BenchChild {
    fn kill(&mut self) -> IoResult<()> {
        Ok(())
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(self.clone())
    }
}

impl Child for BenchChild {
    fn try_wait(&mut self) -> IoResult<Option<ExitStatus>> {
        Ok(None)
    }

    fn wait(&mut self) -> IoResult<ExitStatus> {
        Ok(ExitStatus::with_exit_code(0))
    }

    fn process_id(&self) -> Option<u32> {
        None
    }
}

struct BenchMasterPty;

impl MasterPty for BenchMasterPty {
    fn resize(&self, _size: PtySize) -> Result<(), Error> {
        Ok(())
    }

    fn get_size(&self) -> Result<PtySize, Error> {
        Ok(PtySize::default())
    }

    fn try_clone_reader(&self) -> Result<Box<dyn Read + Send>, Error> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }

    fn take_writer(&self) -> Result<Box<dyn Write + Send>, Error> {
        Ok(Box::new(Vec::<u8>::new()))
    }

    #[cfg(unix)]
    fn process_group_leader(&self) -> Option<libc::pid_t> {
        None
    }

    #[cfg(unix)]
    fn as_raw_fd(&self) -> Option<std::os::fd::RawFd> {
        None
    }

    #[cfg(unix)]
    fn tty_name(&self) -> Option<std::path::PathBuf> {
        None
    }
}

fn make_local_pane() -> LocalPane {
    let terminal = Terminal::new(
        TerminalSize {
            rows: 24,
            cols: 120,
            pixel_width: 960,
            pixel_height: 384,
            dpi: 96,
        },
        Arc::new(BenchTermConfig),
        "frankenterm-mux-event-bus-bench",
        env!("CARGO_PKG_VERSION"),
        Box::new(Vec::<u8>::new()),
    );

    LocalPane::new(
        9001 as PaneId,
        terminal,
        Box::new(BenchChild),
        Box::new(BenchMasterPty),
        Box::new(Vec::<u8>::new()),
        1 as DomainId,
        "bench-localpane".to_string(),
    )
}

fn pane_io_batches() -> Vec<Vec<termwiz::escape::Action>> {
    (0..PANE_IO_BATCH_COUNT)
        .map(|idx| {
            let line = format!("pane-output-{idx:04}: compiling mux event path\r\n");
            line.chars()
                .map(termwiz::escape::Action::Print)
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Benchmark: LocalPane pane-output action application. In the default build
/// this is the direct terminal-mutex path. With `disruptor-pane-io` enabled, the
/// first case measures the feature's uncontended fast path and the feature-only
/// second case forces real ring staging under terminal-lock contention.
fn bench_pane_io_perform_actions(c: &mut Criterion) {
    let batches = pane_io_batches();
    let action_count: usize = batches.iter().map(Vec::len).sum();
    let mut group = c.benchmark_group("pane_io_perform_actions");
    group.throughput(Throughput::Elements(action_count as u64));

    group.bench_function("default_or_uncontended_feature_path", |b| {
        b.iter_batched(
            make_local_pane,
            |pane| {
                for actions in &batches {
                    pane.perform_actions(black_box(actions.clone()));
                }
                black_box(pane.get_cursor_position());
            },
            BatchSize::SmallInput,
        );
    });

    #[cfg(feature = "disruptor-pane-io")]
    group.bench_function("disruptor_staged_under_terminal_lock", |b| {
        b.iter_batched(
            make_local_pane,
            |pane| {
                pane.bench_with_terminal_lock_held(|| {
                    for actions in &batches {
                        pane.perform_actions(black_box(actions.clone()));
                    }
                });
                black_box(pane.get_cursor_position());
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_fire_single_native,
    bench_fire_10_native_handlers,
    bench_fire_mixed_priorities,
    bench_fire_filtered,
    bench_register_deregister,
    bench_fire_pane_text_payload,
    bench_update_status_60hz,
    bench_pane_io_perform_actions,
);
criterion_main!(benches);
