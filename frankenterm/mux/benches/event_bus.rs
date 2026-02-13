//! Criterion benchmarks for the native EventBus dispatch path.
//!
//! Measures native handler dispatch overhead to verify the <1 μs target
//! required by wa-3dfxb.13.

use criterion::{criterion_group, criterion_main, Criterion};
use std::hint::black_box;
use mux::events::{
    Event, EventAction, EventBus, EventPayload, EventType, HandlerFn, HandlerPriority,
};
use std::sync::Arc;

/// Benchmark: fire a single native handler (no filter).
fn bench_fire_single_native(c: &mut Criterion) {
    let bus = EventBus::new();
    let handler: Arc<HandlerFn> = Arc::new(|_| vec![]);
    bus.register(HandlerPriority::Native, None, handler);

    let event = Event::with_timestamp(EventType::PaneOutput, EventPayload::Empty, 0);

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
        bus.register(HandlerPriority::Native, None, handler);
    }

    let event = Event::with_timestamp(EventType::PaneOutput, EventPayload::Empty, 0);

    c.bench_function("fire_10_native_handlers", |b| {
        b.iter(|| bus.fire(black_box(&event)));
    });
}

/// Benchmark: fire with mixed priorities (3 native, 3 wasm, 3 lua).
fn bench_fire_mixed_priorities(c: &mut Criterion) {
    let bus = EventBus::new();
    let handler: Arc<HandlerFn> = Arc::new(|_| vec![]);

    for _ in 0..3 {
        bus.register(HandlerPriority::Native, None, handler.clone());
    }
    for _ in 0..3 {
        bus.register(HandlerPriority::Wasm, None, handler.clone());
    }
    for _ in 0..3 {
        bus.register(HandlerPriority::Lua, None, handler.clone());
    }

    let event = Event::with_timestamp(EventType::PaneOutput, EventPayload::Empty, 0);

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
        );
    }
    // 1 handler for the type we'll fire.
    bus.register(
        HandlerPriority::Native,
        Some(EventType::PaneOutput),
        handler,
    );

    let event = Event::with_timestamp(EventType::PaneOutput, EventPayload::Empty, 0);

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
            let id = bus.register(HandlerPriority::Native, None, handler.clone());
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
    );

    let text: Arc<str> = Arc::from("$ cargo build\n   Compiling mux v0.1.0\n");
    let event = Event::with_timestamp(
        EventType::PaneOutput,
        EventPayload::PaneText {
            pane_id: 42,
            text: text.clone(),
        },
        0,
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
        );
    }

    let event = Event::with_timestamp(
        EventType::UpdateStatus,
        EventPayload::Status { pane_id: 0 },
        0,
    );

    c.bench_function("update_status_3_handlers", |b| {
        b.iter(|| bus.fire(black_box(&event)));
    });
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
);
criterion_main!(benches);
