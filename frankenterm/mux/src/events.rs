//! Native event bus for hot-path event dispatch.
//!
//! Replaces Lua callbacks in performance-critical paths (update-status,
//! pane-output, user-var-changed, window-resized) with typed Rust handlers.
//!
//! Handler PRIORITY ordering is **Native → WASM → Lua**: native handlers run
//! first (with zero allocation overhead), and any scripting-engine handlers
//! would run after them via the `ScriptingEngine` trait's `fire_event` method.
//!
//! NOTE: this bus is currently a benchmark/test substrate rather than a live
//! mux event path. No production code outside this module constructs or fires
//! one of these events, and no production code registers a native, WASM, or Lua
//! handler. The `frankenterm-scripting` crate that could consume the lower
//! tiers is also not wired into mux. The ordering contract is enforced whenever
//! handlers are registered (see the priority tests), but it is not production
//! latency evidence until real producers and consumers are connected.

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::convert::TryFrom;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Event types
// ---------------------------------------------------------------------------

/// Hot-path event types that the native bus dispatches.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    /// Status bar update (fired every render frame, ~60 Hz).
    UpdateStatus,
    /// A user variable changed on a pane.
    UserVarChanged,
    /// Raw pane output received.
    PaneOutput,
    /// A window/pane was resized.
    WindowResized,
    /// A pane gained focus.
    PaneFocused,
    /// A new pane was added to the mux.
    PaneAdded,
    /// A pane was removed from the mux.
    PaneRemoved,
    /// Configuration was reloaded.
    ConfigReloaded,
    /// Caller-defined event type.
    Custom(String),
}

impl std::fmt::Display for EventType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UpdateStatus => f.write_str("update_status"),
            Self::UserVarChanged => f.write_str("user_var_changed"),
            Self::PaneOutput => f.write_str("pane_output"),
            Self::WindowResized => f.write_str("window_resized"),
            Self::PaneFocused => f.write_str("pane_focused"),
            Self::PaneAdded => f.write_str("pane_added"),
            Self::PaneRemoved => f.write_str("pane_removed"),
            Self::ConfigReloaded => f.write_str("config_reloaded"),
            Self::Custom(name) => write!(f, "custom:{name}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Handler priority
// ---------------------------------------------------------------------------

/// Priority level controlling handler dispatch order.
///
/// Lower numeric value = higher priority = dispatched first.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum HandlerPriority {
    /// Rust-native handlers — zero overhead, dispatched first.
    #[default]
    Native = 0,
    /// WASM sandbox handlers — ~100 μs overhead.
    Wasm = 1,
    /// Lua scripting handlers — ~1 ms overhead.
    Lua = 2,
}

// ---------------------------------------------------------------------------
// Event payload
// ---------------------------------------------------------------------------

/// Typed payload carried by events.
///
/// Avoids the `Value` conversion overhead of the scripting layer by using
/// concrete Rust types for the hot paths.
#[derive(Clone, Debug)]
pub enum EventPayload {
    /// No payload.
    Empty,
    /// Simple pane identifier.
    PaneId(u64),
    /// Raw text output from a pane.
    PaneText { pane_id: u64, text: Arc<str> },
    /// Window/pane resize dimensions.
    Resize {
        window_id: u64,
        rows: u16,
        cols: u16,
    },
    /// User variable change on a pane.
    UserVar {
        pane_id: u64,
        key: String,
        value: String,
    },
    /// Status update for a pane.
    Status { pane_id: u64 },
    /// Configuration keys that changed.
    ConfigKeys { keys: Vec<String> },
}

// ---------------------------------------------------------------------------
// Event
// ---------------------------------------------------------------------------

/// Identity of one process-local monotonic clock epoch.
///
/// The random epoch distinguishes process restarts even when the operating
/// system reuses a PID. The PID makes an inherited clock fail closed after a
/// `fork` rather than silently treating the child as the same clock domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EventClockDomain {
    process_epoch: Uuid,
    process_id: u32,
}

impl EventClockDomain {
    /// Construct a non-ambient clock-domain identity.
    #[must_use]
    pub fn new(process_epoch: Uuid, process_id: u32) -> Option<Self> {
        if process_epoch.is_nil() {
            return None;
        }
        Some(Self {
            process_epoch,
            process_id,
        })
    }

    /// Random process-epoch component of this clock domain.
    #[must_use]
    pub fn process_epoch(self) -> Uuid {
        self.process_epoch
    }

    /// Operating-system process ID captured with the epoch.
    #[must_use]
    pub fn process_id(self) -> u32 {
        self.process_id
    }
}

/// Timestamp from one explicit process-local monotonic clock domain.
///
/// Only `monotonic_ns` participates in ordering or duration arithmetic.
/// Wall time is optional diagnostic metadata and is never reclassified as a
/// monotonic value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EventTimestamp {
    pub clock_domain: EventClockDomain,
    pub monotonic_ns: u64,
    pub wall_time_unix_ns: Option<u64>,
}

impl EventTimestamp {
    /// Construct a timestamp from an already-authoritative clock reading.
    #[must_use]
    pub fn from_parts(
        clock_domain: EventClockDomain,
        monotonic_ns: u64,
        wall_time_unix_ns: Option<u64>,
    ) -> Self {
        Self {
            clock_domain,
            monotonic_ns,
            wall_time_unix_ns,
        }
    }

    /// Compute a duration only when both timestamps share a clock domain.
    ///
    /// # Errors
    ///
    /// Returns [`EventClockError::ClockDomainMismatch`] across process epochs
    /// and [`EventClockError::TimestampRegression`] when `self` precedes
    /// `earlier` in the same domain.
    pub fn duration_since(self, earlier: Self) -> Result<Duration, EventClockError> {
        if self.clock_domain != earlier.clock_domain {
            return Err(EventClockError::ClockDomainMismatch {
                earlier: earlier.clock_domain,
                later: self.clock_domain,
            });
        }
        self.monotonic_ns
            .checked_sub(earlier.monotonic_ns)
            .map(Duration::from_nanos)
            .ok_or(EventClockError::TimestampRegression {
                earlier_ns: earlier.monotonic_ns,
                later_ns: self.monotonic_ns,
            })
    }
}

/// Failure to produce or compare authoritative event timestamps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EventClockError {
    /// The monotonic source was sampled before this clock's origin.
    #[error("mux event monotonic clock regressed before its process origin")]
    MonotonicSourceRegression,
    /// Nanoseconds since the process epoch no longer fit the wire-sized field.
    #[error("mux event monotonic timestamp exhausted after {elapsed_ns} nanoseconds")]
    TimestampExhausted { elapsed_ns: u128 },
    /// The process-local clock was inherited by a different process.
    #[error("mux event clock belongs to process {expected}, not process {actual}")]
    ProcessIdentityChanged { expected: u32, actual: u32 },
    /// Duration arithmetic crossed a process restart or fork boundary.
    #[error(
        "mux event timestamps belong to different process clock domains: earlier={earlier:?}, later={later:?}"
    )]
    ClockDomainMismatch {
        earlier: EventClockDomain,
        later: EventClockDomain,
    },
    /// A later observation carried a smaller monotonic timestamp.
    #[error("mux event monotonic timestamp regressed from {earlier_ns} to {later_ns}")]
    TimestampRegression { earlier_ns: u64, later_ns: u64 },
}

#[derive(Debug)]
struct EventClock {
    domain: EventClockDomain,
    origin: Instant,
}

impl EventClock {
    fn process() -> &'static Self {
        static CLOCK: OnceLock<EventClock> = OnceLock::new();
        CLOCK.get_or_init(|| Self {
            domain: EventClockDomain {
                process_epoch: Uuid::new_v4(),
                process_id: std::process::id(),
            },
            origin: Instant::now(),
        })
    }

    fn now(&self) -> Result<EventTimestamp, EventClockError> {
        self.timestamp_at(Instant::now(), SystemTime::now(), std::process::id())
    }

    fn timestamp_at(
        &self,
        monotonic_now: Instant,
        wall_now: SystemTime,
        process_id: u32,
    ) -> Result<EventTimestamp, EventClockError> {
        if process_id != self.domain.process_id {
            return Err(EventClockError::ProcessIdentityChanged {
                expected: self.domain.process_id,
                actual: process_id,
            });
        }
        let elapsed = monotonic_now
            .checked_duration_since(self.origin)
            .ok_or(EventClockError::MonotonicSourceRegression)?;
        self.timestamp_from_elapsed(elapsed, wall_now)
    }

    fn timestamp_from_elapsed(
        &self,
        elapsed: Duration,
        wall_now: SystemTime,
    ) -> Result<EventTimestamp, EventClockError> {
        let elapsed_ns = elapsed.as_nanos();
        let monotonic_ns = u64::try_from(elapsed_ns)
            .map_err(|_| EventClockError::TimestampExhausted { elapsed_ns })?;
        let wall_time_unix_ns = wall_now
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()
            .and_then(|duration| u64::try_from(duration.as_nanos()).ok());
        Ok(EventTimestamp::from_parts(
            self.domain,
            monotonic_ns,
            wall_time_unix_ns,
        ))
    }
}

/// A typed event flowing through the bus.
#[derive(Clone, Debug)]
pub struct Event {
    pub event_type: EventType,
    pub payload: EventPayload,
    pub timestamp: EventTimestamp,
}

impl Event {
    /// Create a new event with the current timestamp.
    ///
    /// # Errors
    ///
    /// Returns an [`EventClockError`] if the process clock was inherited by a
    /// different process, regressed, or exhausted its `u64` nanosecond field.
    pub fn new(event_type: EventType, payload: EventPayload) -> Result<Self, EventClockError> {
        Ok(Self {
            event_type,
            payload,
            timestamp: EventClock::process().now()?,
        })
    }

    /// Create an event with an explicit timestamp (useful for testing).
    #[must_use]
    pub fn with_timestamp(
        event_type: EventType,
        payload: EventPayload,
        timestamp: EventTimestamp,
    ) -> Self {
        Self {
            event_type,
            payload,
            timestamp,
        }
    }
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

/// Actions produced by event handlers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventAction {
    /// Modify a configuration key.
    SetConfig { key: String, value: String },
    /// Send text input to a pane.
    SendInput { pane_id: u64, text: String },
    /// Emit a log message.
    Log { message: String },
    /// Extension-defined action.
    Custom { name: String, data: String },
}

// ---------------------------------------------------------------------------
// Handler registration
// ---------------------------------------------------------------------------

/// Opaque handle returned when registering a handler.
pub type HandlerId = u64;

/// Terminal failure from the handler-ID allocator.
///
/// `u64::MAX` is reserved as a sticky exhausted state. Refusing the
/// registration prevents two live handlers from sharing an opaque ID and
/// preserves one-to-one deregistration semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HandlerRegistrationError {
    /// The process-lifetime handler-ID space has no usable values left.
    #[error("mux event handler ID space is exhausted; create a new event bus")]
    IdExhausted,
}

/// Handler callback signature.
///
/// Receives an immutable reference to the event and returns zero or more
/// actions.  Handlers **must not block** — any I/O or long-running work
/// should be spawned onto a background task.
pub type HandlerFn = dyn Fn(&Event) -> Vec<EventAction> + Send + Sync;

/// Internal record for a registered handler.
struct HandlerRecord {
    id: HandlerId,
    priority: HandlerPriority,
    handler: Arc<HandlerFn>,
    /// If set, this handler only fires for the specified event type.
    filter: Option<EventType>,
}

// ---------------------------------------------------------------------------
// EventBus
// ---------------------------------------------------------------------------

/// The native event bus for hot-path event dispatch.
///
/// Handlers are dispatched in priority order (Native → Wasm → Lua).
/// Within the same priority level, handlers fire in registration order.
///
/// The bus uses a `RwLock` so that event dispatch (read path) can proceed
/// concurrently, while handler registration/deregistration (write path)
/// takes exclusive access.
pub struct EventBus {
    handlers: RwLock<Vec<HandlerRecord>>,
    next_id: AtomicU64,
}

impl EventBus {
    /// Create a new, empty event bus.
    pub fn new() -> Self {
        Self {
            handlers: RwLock::new(Vec::new()),
            next_id: AtomicU64::new(1),
        }
    }

    /// Register a handler with the given priority and optional event filter.
    ///
    /// Returns a `HandlerId` that can be used to deregister the handler.
    ///
    /// # Errors
    ///
    /// Returns [`HandlerRegistrationError::IdExhausted`] without mutating the
    /// handler set when the process-lifetime ID space is exhausted.
    pub fn register(
        &self,
        priority: HandlerPriority,
        filter: Option<EventType>,
        handler: Arc<HandlerFn>,
    ) -> Result<HandlerId, HandlerRegistrationError> {
        let mut handlers = self.handlers.write();
        let id = self.next_handler_id()?;
        let record = HandlerRecord {
            id,
            priority,
            handler,
            filter,
        };
        handlers.push(record);
        // Maintain sort by priority so dispatch is a simple linear scan.
        handlers.sort_by_key(|r| r.priority);
        Ok(id)
    }

    fn next_handler_id(&self) -> Result<HandlerId, HandlerRegistrationError> {
        let mut current = self.next_id.load(Ordering::Relaxed);
        loop {
            let next = current
                .checked_add(1)
                .ok_or(HandlerRegistrationError::IdExhausted)?;
            match self.next_id.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(current),
                Err(observed) => current = observed,
            }
        }
    }

    /// Remove a previously registered handler.
    ///
    /// Returns `true` if the handler was found and removed.
    pub fn deregister(&self, id: HandlerId) -> bool {
        let mut handlers = self.handlers.write();
        let before = handlers.len();
        handlers.retain(|r| r.id != id);
        handlers.len() < before
    }

    /// Fire an event and collect actions from all matching handlers.
    ///
    /// Handlers are invoked in priority order (Native first, then Wasm,
    /// then Lua).  Within the same priority, registration order is preserved.
    pub fn fire(&self, event: &Event) -> Vec<EventAction> {
        let handlers = self.matching_handlers(&event.event_type);
        let mut actions = Vec::new();
        for handler in handlers {
            let mut handler_actions = handler(event);
            actions.append(&mut handler_actions);
        }
        actions
    }

    /// Return the number of currently registered handlers.
    pub fn handler_count(&self) -> usize {
        self.handlers.read().len()
    }

    /// Return the number of handlers registered for a specific event type.
    pub fn handler_count_for(&self, event_type: &EventType) -> usize {
        self.handlers
            .read()
            .iter()
            .filter(|r| handler_matches_event_type(r, event_type))
            .count()
    }

    /// Return all registered handler IDs (useful for diagnostics).
    pub fn handler_ids(&self) -> Vec<HandlerId> {
        self.handlers.read().iter().map(|r| r.id).collect()
    }

    /// Fire an event and return a summary: (action_count, handler_count_matched).
    pub fn fire_counted(&self, event: &Event) -> (usize, usize) {
        let handlers = self.matching_handlers(&event.event_type);
        let mut action_count = 0;
        for handler in &handlers {
            action_count += handler(event).len();
        }
        (action_count, handlers.len())
    }

    /// Remove all handlers, returning the count removed.
    pub fn clear(&self) -> usize {
        let mut handlers = self.handlers.write();
        let count = handlers.len();
        handlers.clear();
        count
    }

    fn matching_handlers(&self, event_type: &EventType) -> Vec<Arc<HandlerFn>> {
        self.handlers
            .read()
            .iter()
            .filter(|record| handler_matches_event_type(record, event_type))
            .map(|record| Arc::clone(&record.handler))
            .collect()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

fn handler_matches_event_type(record: &HandlerRecord, event_type: &EventType) -> bool {
    record
        .filter
        .as_ref()
        .is_none_or(|filter| filter == event_type)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_timestamp(monotonic_ns: u64) -> EventTimestamp {
        EventTimestamp::from_parts(
            EventClockDomain::new(Uuid::from_u128(1), 7).expect("fixture clock domain is non-nil"),
            monotonic_ns,
            None,
        )
    }

    fn make_event(event_type: EventType) -> Event {
        Event::with_timestamp(event_type, EventPayload::Empty, test_timestamp(0))
    }

    #[test]
    fn register_and_fire_single_handler() {
        let bus = EventBus::new();
        let handler: Arc<HandlerFn> = Arc::new(|_| {
            vec![EventAction::Log {
                message: "fired".into(),
            }]
        });
        let _id = bus
            .register(HandlerPriority::Native, None, handler)
            .expect("handler registration should succeed");

        let actions = bus.fire(&make_event(EventType::PaneOutput));
        assert_eq!(actions.len(), 1);
        assert_eq!(
            actions[0],
            EventAction::Log {
                message: "fired".into()
            }
        );
    }

    #[test]
    fn handler_id_allocator_fails_closed_without_mutating_handlers() {
        let bus = EventBus::new();
        bus.next_id.store(u64::MAX - 1, Ordering::Relaxed);
        let handler: Arc<HandlerFn> = Arc::new(|_| vec![]);

        let last = bus
            .register(HandlerPriority::Native, None, Arc::clone(&handler))
            .expect("MAX - 1 is the last usable handler ID");
        assert_eq!(last, u64::MAX - 1);
        assert_eq!(bus.next_id.load(Ordering::Relaxed), u64::MAX);

        assert_eq!(
            bus.register(HandlerPriority::Native, None, handler),
            Err(HandlerRegistrationError::IdExhausted)
        );
        assert_eq!(bus.handler_ids(), vec![last]);
        assert!(bus.deregister(last));
        assert_eq!(bus.handler_count(), 0);
        assert_eq!(
            bus.next_handler_id(),
            Err(HandlerRegistrationError::IdExhausted)
        );
        assert_eq!(bus.next_id.load(Ordering::Relaxed), u64::MAX);
    }

    #[test]
    fn concurrent_handler_id_exhaustion_admits_exactly_one_last_handler() {
        let bus = Arc::new(EventBus::new());
        bus.next_id.store(u64::MAX - 1, Ordering::Relaxed);

        let mut threads = Vec::new();
        for _ in 0..8 {
            let bus = Arc::clone(&bus);
            threads.push(std::thread::spawn(move || {
                let handler: Arc<HandlerFn> = Arc::new(|_| vec![]);
                bus.register(HandlerPriority::Native, None, handler)
            }));
        }

        let results: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().expect("registration thread should not panic"))
            .collect();
        assert_eq!(
            results
                .iter()
                .filter(|result| **result == Ok(u64::MAX - 1))
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|result| **result == Err(HandlerRegistrationError::IdExhausted))
                .count(),
            7
        );
        assert_eq!(bus.handler_ids(), vec![u64::MAX - 1]);
        assert_eq!(bus.next_id.load(Ordering::Relaxed), u64::MAX);
    }

    #[test]
    fn deregister_removes_handler() {
        let bus = EventBus::new();
        let handler: Arc<HandlerFn> = Arc::new(|_| {
            vec![EventAction::Log {
                message: "fired".into(),
            }]
        });
        let id = bus
            .register(HandlerPriority::Native, None, handler)
            .expect("handler registration should succeed");
        assert_eq!(bus.handler_count(), 1);

        assert!(bus.deregister(id));
        assert_eq!(bus.handler_count(), 0);

        let actions = bus.fire(&make_event(EventType::PaneOutput));
        assert!(actions.is_empty());
    }

    #[test]
    fn deregister_nonexistent_returns_false() {
        let bus = EventBus::new();
        assert!(!bus.deregister(999));
    }

    #[test]
    fn filter_restricts_handler_to_event_type() {
        let bus = EventBus::new();
        let handler: Arc<HandlerFn> = Arc::new(|_| {
            vec![EventAction::Log {
                message: "pane_output".into(),
            }]
        });
        let _id = bus
            .register(
                HandlerPriority::Native,
                Some(EventType::PaneOutput),
                handler,
            )
            .expect("handler registration should succeed");

        // Should fire for PaneOutput.
        let actions = bus.fire(&make_event(EventType::PaneOutput));
        assert_eq!(actions.len(), 1);

        // Should NOT fire for UpdateStatus.
        let actions = bus.fire(&make_event(EventType::UpdateStatus));
        assert!(actions.is_empty());
    }

    #[test]
    fn priority_order_native_before_wasm_before_lua() {
        let bus = EventBus::new();

        // Register in reverse order to prove sorting works.
        let lua_handler: Arc<HandlerFn> = Arc::new(|_| {
            vec![EventAction::Custom {
                name: "lua".into(),
                data: String::new(),
            }]
        });
        let wasm_handler: Arc<HandlerFn> = Arc::new(|_| {
            vec![EventAction::Custom {
                name: "wasm".into(),
                data: String::new(),
            }]
        });
        let native_handler: Arc<HandlerFn> = Arc::new(|_| {
            vec![EventAction::Custom {
                name: "native".into(),
                data: String::new(),
            }]
        });

        bus.register(HandlerPriority::Lua, None, lua_handler)
            .expect("handler registration should succeed");
        bus.register(HandlerPriority::Wasm, None, wasm_handler)
            .expect("handler registration should succeed");
        bus.register(HandlerPriority::Native, None, native_handler)
            .expect("handler registration should succeed");

        let actions = bus.fire(&make_event(EventType::PaneOutput));
        assert_eq!(actions.len(), 3);

        let names: Vec<&str> = actions
            .iter()
            .map(|a| match a {
                EventAction::Custom { name, .. } => name.as_str(),
                _ => "",
            })
            .collect();
        assert_eq!(names, vec!["native", "wasm", "lua"]);
    }

    #[test]
    fn handler_ids_returns_all_registered() {
        let bus = EventBus::new();
        let h: Arc<HandlerFn> = Arc::new(|_| vec![]);
        let id1 = bus
            .register(HandlerPriority::Native, None, h.clone())
            .expect("handler registration should succeed");
        let id2 = bus
            .register(HandlerPriority::Wasm, None, h.clone())
            .expect("handler registration should succeed");
        let id3 = bus
            .register(HandlerPriority::Lua, None, h)
            .expect("handler registration should succeed");

        let ids = bus.handler_ids();
        assert!(ids.contains(&id1));
        assert!(ids.contains(&id2));
        assert!(ids.contains(&id3));
    }

    #[test]
    fn clear_removes_all_handlers() {
        let bus = EventBus::new();
        let h: Arc<HandlerFn> = Arc::new(|_| vec![]);
        bus.register(HandlerPriority::Native, None, h.clone())
            .expect("handler registration should succeed");
        bus.register(HandlerPriority::Wasm, None, h)
            .expect("handler registration should succeed");
        assert_eq!(bus.handler_count(), 2);

        let removed = bus.clear();
        assert_eq!(removed, 2);
        assert_eq!(bus.handler_count(), 0);
    }

    #[test]
    fn handler_count_for_filters_correctly() {
        let bus = EventBus::new();
        let h: Arc<HandlerFn> = Arc::new(|_| vec![]);
        bus.register(
            HandlerPriority::Native,
            Some(EventType::PaneOutput),
            h.clone(),
        )
        .expect("handler registration should succeed");
        bus.register(
            HandlerPriority::Native,
            Some(EventType::UpdateStatus),
            h.clone(),
        )
        .expect("handler registration should succeed");
        bus.register(HandlerPriority::Native, None, h)
            .expect("handler registration should succeed"); // wildcard

        // PaneOutput: 1 specific + 1 wildcard = 2
        assert_eq!(bus.handler_count_for(&EventType::PaneOutput), 2);
        // UpdateStatus: 1 specific + 1 wildcard = 2
        assert_eq!(bus.handler_count_for(&EventType::UpdateStatus), 2);
        // PaneAdded: 0 specific + 1 wildcard = 1
        assert_eq!(bus.handler_count_for(&EventType::PaneAdded), 1);
    }

    #[test]
    fn event_type_display() {
        assert_eq!(EventType::UpdateStatus.to_string(), "update_status");
        assert_eq!(EventType::PaneOutput.to_string(), "pane_output");
        assert_eq!(
            EventType::Custom("my_event".into()).to_string(),
            "custom:my_event"
        );
    }

    #[test]
    fn event_type_serde_roundtrip() {
        let types = vec![
            EventType::UpdateStatus,
            EventType::PaneOutput,
            EventType::WindowResized,
            EventType::Custom("test".into()),
        ];
        for et in types {
            let json = serde_json::to_string(&et).unwrap();
            let back: EventType = serde_json::from_str(&json).unwrap();
            assert_eq!(back, et);
        }
    }

    #[test]
    fn handler_priority_serde_roundtrip() {
        for p in [
            HandlerPriority::Native,
            HandlerPriority::Wasm,
            HandlerPriority::Lua,
        ] {
            let json = serde_json::to_string(&p).unwrap();
            let back: HandlerPriority = serde_json::from_str(&json).unwrap();
            assert_eq!(back, p);
        }
    }

    #[test]
    fn handler_priority_ordering() {
        assert!(HandlerPriority::Native < HandlerPriority::Wasm);
        assert!(HandlerPriority::Wasm < HandlerPriority::Lua);
    }

    #[test]
    fn fire_counted_reports_correct_stats() {
        let bus = EventBus::new();
        let h: Arc<HandlerFn> = Arc::new(|_| {
            vec![
                EventAction::Log {
                    message: "a".into(),
                },
                EventAction::Log {
                    message: "b".into(),
                },
            ]
        });
        bus.register(HandlerPriority::Native, None, h)
            .expect("handler registration should succeed");

        let (action_count, matched) = bus.fire_counted(&make_event(EventType::PaneOutput));
        assert_eq!(action_count, 2);
        assert_eq!(matched, 1);
    }

    #[test]
    fn handler_can_deregister_itself_during_fire() {
        let bus = Arc::new(EventBus::new());
        let handler_id = Arc::new(std::sync::Mutex::new(None));

        let bus_for_handler = Arc::clone(&bus);
        let handler_id_for_handler = Arc::clone(&handler_id);
        let handler: Arc<HandlerFn> = Arc::new(move |_| {
            if let Some(id) = *handler_id_for_handler.lock().unwrap() {
                assert!(bus_for_handler.deregister(id));
            }
            vec![EventAction::Log {
                message: "removed".into(),
            }]
        });

        let id = bus
            .register(HandlerPriority::Native, None, handler)
            .expect("handler registration should succeed");
        *handler_id.lock().unwrap() = Some(id);

        let (tx, rx) = std::sync::mpsc::channel();
        let bus_for_thread = Arc::clone(&bus);
        let handle = std::thread::spawn(move || {
            let actions = bus_for_thread.fire(&make_event(EventType::PaneOutput));
            tx.send(actions.len()).unwrap();
        });

        let action_count = rx
            .recv_timeout(std::time::Duration::from_millis(200))
            .expect("dispatch should complete without deadlock");
        handle.join().expect("dispatch thread should not panic");
        assert_eq!(action_count, 1);
        assert_eq!(bus.handler_count(), 0);
    }

    #[test]
    fn handler_can_deregister_itself_during_fire_counted() {
        let bus = Arc::new(EventBus::new());
        let handler_id = Arc::new(std::sync::Mutex::new(None));

        let bus_for_handler = Arc::clone(&bus);
        let handler_id_for_handler = Arc::clone(&handler_id);
        let handler: Arc<HandlerFn> = Arc::new(move |_| {
            if let Some(id) = *handler_id_for_handler.lock().unwrap() {
                assert!(bus_for_handler.deregister(id));
            }
            vec![EventAction::Log {
                message: "removed".into(),
            }]
        });

        let id = bus
            .register(HandlerPriority::Native, None, handler)
            .expect("handler registration should succeed");
        *handler_id.lock().unwrap() = Some(id);

        let (tx, rx) = std::sync::mpsc::channel();
        let bus_for_thread = Arc::clone(&bus);
        let handle = std::thread::spawn(move || {
            let stats = bus_for_thread.fire_counted(&make_event(EventType::PaneOutput));
            tx.send(stats).unwrap();
        });

        let (action_count, matched) = rx
            .recv_timeout(std::time::Duration::from_millis(200))
            .expect("dispatch should complete without deadlock");
        handle.join().expect("dispatch thread should not panic");
        assert_eq!(action_count, 1);
        assert_eq!(matched, 1);
        assert_eq!(bus.handler_count(), 0);
    }

    #[test]
    fn event_with_timestamp_preserves_value() {
        let timestamp = test_timestamp(42);
        let event = Event::with_timestamp(EventType::PaneOutput, EventPayload::Empty, timestamp);
        assert_eq!(event.timestamp, timestamp);
        assert_eq!(event.event_type, EventType::PaneOutput);
    }

    #[test]
    fn event_clock_domain_requires_a_non_nil_process_epoch() {
        assert_eq!(EventClockDomain::new(Uuid::nil(), 7), None);
        let epoch = Uuid::from_u128(1);
        let domain = EventClockDomain::new(epoch, 7).expect("fixture clock domain is non-nil");
        assert_eq!(domain.process_epoch(), epoch);
        assert_eq!(domain.process_id(), 7);
    }

    #[test]
    fn process_clock_samples_share_a_domain_and_never_regress() {
        let clock = EventClock::process();
        let first = clock.now().expect("first process-clock sample");
        let second = clock.now().expect("second process-clock sample");

        assert_eq!(first.clock_domain, clock.domain);
        assert_eq!(second.clock_domain, clock.domain);
        assert!(second.duration_since(first).is_ok());
    }

    #[test]
    fn event_clock_rejects_source_regression_and_process_change() {
        let origin = Instant::now();
        let domain =
            EventClockDomain::new(Uuid::from_u128(2), 7).expect("fixture clock domain is non-nil");
        let clock = EventClock { domain, origin };
        let before_origin = origin
            .checked_sub(Duration::from_nanos(1))
            .expect("fixture instant supports a one-nanosecond subtraction");

        assert_eq!(
            clock.timestamp_at(before_origin, SystemTime::UNIX_EPOCH, 7),
            Err(EventClockError::MonotonicSourceRegression)
        );
        assert_eq!(
            clock.timestamp_at(origin, SystemTime::UNIX_EPOCH, 8),
            Err(EventClockError::ProcessIdentityChanged {
                expected: 7,
                actual: 8,
            })
        );
    }

    #[test]
    fn event_clock_reserves_overflow_as_typed_exhaustion() {
        let clock = EventClock {
            domain: EventClockDomain::new(Uuid::from_u128(3), 7)
                .expect("fixture clock domain is non-nil"),
            origin: Instant::now(),
        };
        let elapsed = Duration::from_secs(u64::MAX);

        assert_eq!(
            clock.timestamp_from_elapsed(elapsed, SystemTime::UNIX_EPOCH),
            Err(EventClockError::TimestampExhausted {
                elapsed_ns: elapsed.as_nanos(),
            })
        );
    }

    #[test]
    fn wall_time_is_optional_metadata_and_never_orders_events() {
        let clock = EventClock {
            domain: EventClockDomain::new(Uuid::from_u128(4), 7)
                .expect("fixture clock domain is non-nil"),
            origin: Instant::now(),
        };
        let pre_epoch = SystemTime::UNIX_EPOCH
            .checked_sub(Duration::from_secs(1))
            .expect("fixture wall clock supports a one-second subtraction");
        let before = clock
            .timestamp_from_elapsed(Duration::from_nanos(1), pre_epoch)
            .expect("monotonic fixture is representable");
        let after = clock
            .timestamp_from_elapsed(
                Duration::from_nanos(2),
                SystemTime::UNIX_EPOCH + Duration::from_nanos(5),
            )
            .expect("monotonic fixture is representable");

        assert_eq!(before.wall_time_unix_ns, None);
        assert_eq!(after.wall_time_unix_ns, Some(5));
        assert_eq!(after.duration_since(before), Ok(Duration::from_nanos(1)));
    }

    #[test]
    fn backwards_wall_clock_does_not_regress_monotonic_time() {
        let clock = EventClock {
            domain: EventClockDomain::new(Uuid::from_u128(7), 7)
                .expect("fixture clock domain is non-nil"),
            origin: Instant::now(),
        };
        let before = clock
            .timestamp_from_elapsed(
                Duration::from_nanos(10),
                SystemTime::UNIX_EPOCH + Duration::from_secs(2),
            )
            .expect("first monotonic fixture is representable");
        let after = clock
            .timestamp_from_elapsed(
                Duration::from_nanos(11),
                SystemTime::UNIX_EPOCH + Duration::from_secs(1),
            )
            .expect("second monotonic fixture is representable");

        assert!(after.wall_time_unix_ns < before.wall_time_unix_ns);
        assert_eq!(after.duration_since(before), Ok(Duration::from_nanos(1)));
    }

    #[test]
    fn timestamps_from_different_process_epochs_are_incomparable() {
        let earlier = EventTimestamp::from_parts(
            EventClockDomain::new(Uuid::from_u128(5), 7).expect("fixture clock domain is non-nil"),
            100,
            None,
        );
        let later = EventTimestamp::from_parts(
            EventClockDomain::new(Uuid::from_u128(6), 7).expect("fixture clock domain is non-nil"),
            200,
            None,
        );

        assert!(matches!(
            later.duration_since(earlier),
            Err(EventClockError::ClockDomainMismatch { .. })
        ));
    }

    #[test]
    fn same_domain_timestamp_regression_is_rejected() {
        let later = test_timestamp(99);
        let earlier = test_timestamp(100);

        assert_eq!(
            later.duration_since(earlier),
            Err(EventClockError::TimestampRegression {
                earlier_ns: 100,
                later_ns: 99,
            })
        );
    }

    #[test]
    fn multiple_handlers_same_priority_fire_in_registration_order() {
        let bus = EventBus::new();
        let results = Arc::new(std::sync::Mutex::new(Vec::new()));

        for i in 0..5u64 {
            let results = results.clone();
            let handler: Arc<HandlerFn> = Arc::new(move |_| {
                results.lock().unwrap().push(i);
                vec![]
            });
            bus.register(HandlerPriority::Native, None, handler)
                .expect("handler registration should succeed");
        }

        bus.fire(&make_event(EventType::PaneOutput));

        let order = results.lock().unwrap().clone();
        assert_eq!(order, vec![0, 1, 2, 3, 4]);
    }

    // ===================================================================
    // Property-based tests
    // ===================================================================

    use proptest::prelude::*;
    use std::collections::HashSet;

    /// Strategy for generating arbitrary event types.
    fn arb_event_type() -> impl Strategy<Value = EventType> {
        prop_oneof![
            Just(EventType::UpdateStatus),
            Just(EventType::UserVarChanged),
            Just(EventType::PaneOutput),
            Just(EventType::WindowResized),
            Just(EventType::PaneFocused),
            Just(EventType::PaneAdded),
            Just(EventType::PaneRemoved),
            Just(EventType::ConfigReloaded),
            "[a-z_]{3,15}".prop_map(EventType::Custom),
        ]
    }

    /// Strategy for generating arbitrary handler priorities.
    fn arb_priority() -> impl Strategy<Value = HandlerPriority> {
        prop_oneof![
            Just(HandlerPriority::Native),
            Just(HandlerPriority::Wasm),
            Just(HandlerPriority::Lua),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(50))]

        /// EventType serde roundtrip preserves all variants.
        #[test]
        fn prop_event_type_serde(et in arb_event_type()) {
            let json = serde_json::to_string(&et).unwrap();
            let back: EventType = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(back, et);
        }

        /// EventType serialization is deterministic.
        #[test]
        fn prop_event_type_deterministic(et in arb_event_type()) {
            let j1 = serde_json::to_string(&et).unwrap();
            let j2 = serde_json::to_string(&et).unwrap();
            prop_assert_eq!(&j1, &j2);
        }

        /// HandlerPriority serde roundtrip.
        #[test]
        fn prop_priority_serde(p in arb_priority()) {
            let json = serde_json::to_string(&p).unwrap();
            let back: HandlerPriority = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(back, p);
        }

        /// HandlerPriority ordering: Native < Wasm < Lua always holds.
        #[test]
        fn prop_priority_ordering(
            a in arb_priority(),
            b in arb_priority(),
        ) {
            // The Ord impl must be consistent with the enum discriminants.
            let a_val = a as u8;
            let b_val = b as u8;
            if a_val < b_val {
                prop_assert!(a < b);
            } else if a_val == b_val {
                prop_assert_eq!(a, b);
            } else {
                prop_assert!(a > b);
            }
        }

        /// Handler IDs are always unique across registrations.
        #[test]
        fn prop_handler_ids_unique(
            count in 1_usize..50,
        ) {
            let bus = EventBus::new();
            let h: Arc<HandlerFn> = Arc::new(|_| vec![]);
            let mut ids = HashSet::new();
            for _ in 0..count {
                let id = bus
                    .register(HandlerPriority::Native, None, h.clone())
                    .expect("handler registration should succeed");
                let inserted = ids.insert(id);
                prop_assert!(inserted, "handler id was reused");
            }
            prop_assert_eq!(bus.handler_count(), count);
        }

        /// handler_count reflects registration and deregistration.
        #[test]
        fn prop_handler_count_tracks_registrations(
            ops in prop::collection::vec(any::<bool>(), 1..100),
        ) {
            let bus = EventBus::new();
            let h: Arc<HandlerFn> = Arc::new(|_| vec![]);
            let mut registered: Vec<HandlerId> = Vec::new();

            for register in ops {
                if register {
                    let id = bus
                        .register(HandlerPriority::Native, None, h.clone())
                        .expect("handler registration should succeed");
                    registered.push(id);
                } else if let Some(id) = registered.pop() {
                    bus.deregister(id);
                }
                prop_assert_eq!(bus.handler_count(), registered.len());
            }
        }

        /// Deregistered handlers produce no actions.
        #[test]
        fn prop_deregistered_handlers_silent(
            n_register in 1_usize..20,
            n_deregister_indices in prop::collection::vec(0_usize..100, 0..20),
        ) {
            let bus = EventBus::new();
            let h: Arc<HandlerFn> = Arc::new(|_| {
                vec![EventAction::Log { message: "x".into() }]
            });

            let mut ids: Vec<HandlerId> = Vec::new();
            for _ in 0..n_register {
                ids.push(
                    bus.register(HandlerPriority::Native, None, h.clone())
                        .expect("handler registration should succeed"),
                );
            }

            let mut removed = 0_usize;
            for idx in &n_deregister_indices {
                if !ids.is_empty() {
                    let real_idx = idx % ids.len();
                    let id = ids.remove(real_idx);
                    if bus.deregister(id) {
                        removed += 1;
                    }
                }
            }

            let actions = bus.fire(&make_event(EventType::PaneOutput));
            let expected_remaining = n_register.saturating_sub(removed);
            prop_assert_eq!(actions.len(), expected_remaining);
        }

        /// Filtered handlers only fire for their event type.
        #[test]
        fn prop_filter_correctness(
            filter_type in arb_event_type(),
            fire_type in arb_event_type(),
        ) {
            let bus = EventBus::new();
            let h: Arc<HandlerFn> = Arc::new(|_| {
                vec![EventAction::Log { message: "hit".into() }]
            });
            bus.register(HandlerPriority::Native, Some(filter_type.clone()), h)
                .expect("handler registration should succeed");

            let actions = bus.fire(&make_event(fire_type.clone()));
            let should_fire = filter_type == fire_type;
            prop_assert_eq!(actions.is_empty(), !should_fire);
        }

        /// Wildcard handlers fire for all event types.
        #[test]
        fn prop_wildcard_always_fires(
            fire_type in arb_event_type(),
        ) {
            let bus = EventBus::new();
            let h: Arc<HandlerFn> = Arc::new(|_| {
                vec![EventAction::Log { message: "wild".into() }]
            });
            bus.register(HandlerPriority::Native, None, h)
                .expect("handler registration should succeed");

            let actions = bus.fire(&make_event(fire_type));
            prop_assert_eq!(actions.len(), 1);
        }

        /// Priority dispatch order is always Native → Wasm → Lua
        /// regardless of registration order.
        #[test]
        fn prop_priority_dispatch_order(
            registration_order in prop::collection::vec(arb_priority(), 3..20),
        ) {
            let bus = EventBus::new();

            for (i, priority) in registration_order.iter().enumerate() {
                let tag = i as u64;
                let p = *priority;
                let handler: Arc<HandlerFn> = Arc::new(move |_| {
                    vec![EventAction::Custom {
                        name: format!("{tag}"),
                        data: format!("{}", p as u8),
                    }]
                });
                bus.register(*priority, None, handler)
                    .expect("handler registration should succeed");
            }

            let actions = bus.fire(&make_event(EventType::PaneOutput));
            prop_assert_eq!(actions.len(), registration_order.len());

            // Check that priorities are non-decreasing in the output.
            let mut last_priority = 0_u8;
            for action in &actions {
                if let EventAction::Custom { data, .. } = action {
                    let p: u8 = data.parse().unwrap();
                    let ok = p >= last_priority;
                    prop_assert!(ok, "dispatch order violated: got {} after {}", p, last_priority);
                    last_priority = p;
                }
            }
        }

        /// Full lifecycle property: register, fire, deregister, fire again.
        /// After deregistration, the handler must not produce actions.
        #[test]
        fn prop_full_lifecycle(
            ops in prop::collection::vec(0_u8..3, 1..200),
        ) {
            let bus = EventBus::new();
            let mut active_ids: Vec<(u64, HandlerId)> = Vec::new();
            let mut active_tokens: HashSet<u64> = HashSet::new();
            let mut next_token: u64 = 1;
            let mut seen_handler_ids: HashSet<HandlerId> = HashSet::new();

            for code in ops {
                match code % 3 {
                    // Register
                    0 => {
                        let token = next_token;
                        next_token += 1;
                        let handler: Arc<HandlerFn> = Arc::new(move |_| {
                            vec![EventAction::Custom {
                                name: format!("{token}"),
                                data: String::new(),
                            }]
                        });
                        let id = bus
                            .register(HandlerPriority::Native, None, handler)
                            .expect("handler registration should succeed");
                        let is_new = seen_handler_ids.insert(id);
                        prop_assert!(is_new, "handler id reused");
                        active_ids.push((token, id));
                        active_tokens.insert(token);
                    }
                    // Deregister
                    1 => {
                        if active_ids.is_empty() {
                            continue;
                        }
                        let idx = (code as usize) % active_ids.len();
                        let (token, id) = active_ids.remove(idx);
                        bus.deregister(id);
                        active_tokens.remove(&token);
                    }
                    // Fire and verify
                    _ => {
                        let actions = bus.fire(&make_event(EventType::PaneOutput));
                        let observed: HashSet<u64> = actions
                            .into_iter()
                            .map(|a| match a {
                                EventAction::Custom { name, .. } => name.parse().unwrap(),
                                _ => panic!("unexpected action"),
                            })
                            .collect();
                        prop_assert_eq!(observed, active_tokens.clone());
                    }
                }
            }
        }

        /// clear() returns the correct count and empties the bus.
        #[test]
        fn prop_clear_returns_count(count in 0_usize..50) {
            let bus = EventBus::new();
            let h: Arc<HandlerFn> = Arc::new(|_| vec![]);
            for _ in 0..count {
                bus.register(HandlerPriority::Native, None, h.clone())
                    .expect("handler registration should succeed");
            }
            let removed = bus.clear();
            prop_assert_eq!(removed, count);
            prop_assert_eq!(bus.handler_count(), 0);
        }

        /// fire_counted returns consistent action_count and matched count.
        #[test]
        fn prop_fire_counted_consistent(
            n_matching in 0_usize..10,
            n_nonmatching in 0_usize..10,
        ) {
            let bus = EventBus::new();
            let h: Arc<HandlerFn> = Arc::new(|_| {
                vec![EventAction::Log { message: "a".into() }]
            });

            for _ in 0..n_matching {
                bus.register(
                    HandlerPriority::Native,
                    Some(EventType::PaneOutput),
                    h.clone(),
                )
                .expect("handler registration should succeed");
            }
            // Also add a wildcard handler for each matching handler.
            let n_wildcard = n_matching;
            for _ in 0..n_wildcard {
                bus.register(HandlerPriority::Native, None, h.clone())
                    .expect("handler registration should succeed");
            }
            for _ in 0..n_nonmatching {
                bus.register(
                    HandlerPriority::Native,
                    Some(EventType::UpdateStatus),
                    h.clone(),
                )
                .expect("handler registration should succeed");
            }

            let (action_count, matched) =
                bus.fire_counted(&make_event(EventType::PaneOutput));

            // Matching = specific PaneOutput handlers + wildcard handlers.
            prop_assert_eq!(matched, n_matching + n_wildcard);
            // Each matched handler produces exactly 1 action.
            prop_assert_eq!(action_count, matched);
        }

        /// EventType Display output matches serde snake_case for built-in variants.
        #[test]
        fn prop_event_type_display_matches_serde(et in arb_event_type()) {
            let display = et.to_string();
            // Display should always produce a non-empty string.
            prop_assert!(!display.is_empty());
            // For non-Custom variants, Display should match the serde name.
            if let EventType::Custom(ref name) = et {
                let expected = format!("custom:{name}");
                prop_assert_eq!(&display, &expected);
            } else {
                let json = serde_json::to_string(&et).unwrap();
                // json is like "\"update_status\"", strip quotes
                let serde_name = json.trim_matches('"');
                prop_assert_eq!(&display, serde_name);
            }
        }

        /// Event::new binds each sample to the process clock domain.
        #[test]
        fn prop_event_new_has_timestamp(_dummy in 0..1_u8) {
            let event = Event::new(EventType::PaneOutput, EventPayload::Empty)
                .expect("process clock should produce a timestamp");
            prop_assert_eq!(
                event.timestamp.clock_domain,
                EventClock::process().domain,
            );
        }

        /// handler_count_for is consistent with handler_count.
        #[test]
        fn prop_handler_count_for_le_total(
            n_filtered in 0_usize..5,
            n_wildcard in 0_usize..5,
            n_other in 0_usize..5,
        ) {
            let bus = EventBus::new();
            let h: Arc<HandlerFn> = Arc::new(|_| vec![]);
            for _ in 0..n_filtered {
                bus.register(
                    HandlerPriority::Native,
                    Some(EventType::PaneOutput),
                    h.clone(),
                )
                .expect("handler registration should succeed");
            }
            for _ in 0..n_wildcard {
                bus.register(HandlerPriority::Native, None, h.clone())
                    .expect("handler registration should succeed");
            }
            for _ in 0..n_other {
                bus.register(
                    HandlerPriority::Native,
                    Some(EventType::UpdateStatus),
                    h.clone(),
                )
                .expect("handler registration should succeed");
            }

            let total = bus.handler_count();
            let for_pane_output = bus.handler_count_for(&EventType::PaneOutput);
            prop_assert_eq!(total, n_filtered + n_wildcard + n_other);
            prop_assert_eq!(for_pane_output, n_filtered + n_wildcard);
            let lte = for_pane_output <= total;
            prop_assert!(lte);
        }
    }

    // ===================================================================
    // Concurrent stress tests
    // ===================================================================

    use std::sync::Barrier;
    use std::sync::atomic::AtomicUsize;

    /// Multiple threads fire events concurrently on a shared bus.
    /// Verifies no panics, no lost actions, and handler count is consistent.
    #[test]
    fn concurrent_fire_from_multiple_threads() {
        let bus = Arc::new(EventBus::new());
        let fire_count = Arc::new(AtomicUsize::new(0));

        let handler: Arc<HandlerFn> = Arc::new(|_| {
            vec![EventAction::Log {
                message: "hit".into(),
            }]
        });
        bus.register(HandlerPriority::Native, None, handler)
            .expect("handler registration should succeed");

        let n_threads = 8;
        let n_fires_per_thread = 1000;
        let barrier = Arc::new(Barrier::new(n_threads));

        let threads: Vec<_> = (0..n_threads)
            .map(|_| {
                let bus = bus.clone();
                let fire_count = fire_count.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    for _ in 0..n_fires_per_thread {
                        let actions = bus.fire(&Event::with_timestamp(
                            EventType::PaneOutput,
                            EventPayload::Empty,
                            test_timestamp(0),
                        ));
                        fire_count.fetch_add(actions.len(), Ordering::Relaxed);
                    }
                })
            })
            .collect();

        for t in threads {
            t.join().unwrap();
        }

        // Each thread fires n_fires_per_thread events, each producing 1 action.
        assert_eq!(
            fire_count.load(Ordering::Relaxed),
            n_threads * n_fires_per_thread
        );
        // Handler is still registered.
        assert_eq!(bus.handler_count(), 1);
    }

    /// Concurrent register + fire: some threads register handlers while
    /// others fire events.  No panics or data corruption should occur.
    #[test]
    fn concurrent_register_and_fire() {
        let bus = Arc::new(EventBus::new());
        let n_registrars = 4;
        let n_firers = 4;
        let n_ops = 500;
        let barrier = Arc::new(Barrier::new(n_registrars + n_firers));

        let total_actions = Arc::new(AtomicUsize::new(0));

        let mut threads: Vec<std::thread::JoinHandle<()>> = Vec::new();

        // Registrar threads.
        for _ in 0..n_registrars {
            let bus = bus.clone();
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                let h: Arc<HandlerFn> = Arc::new(|_| {
                    vec![EventAction::Log {
                        message: "x".into(),
                    }]
                });
                bus.register(HandlerPriority::Native, None, h)
                    .expect("initial handler registration should succeed");
                barrier.wait();
                for _ in 1..n_ops {
                    let h: Arc<HandlerFn> = Arc::new(|_| {
                        vec![EventAction::Log {
                            message: "x".into(),
                        }]
                    });
                    bus.register(HandlerPriority::Native, None, h)
                        .expect("handler registration should succeed");
                }
            }));
        }

        // Firer threads.
        for _ in 0..n_firers {
            let bus = bus.clone();
            let barrier = barrier.clone();
            let total_actions = total_actions.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..n_ops {
                    let actions = bus.fire(&Event::with_timestamp(
                        EventType::PaneOutput,
                        EventPayload::Empty,
                        test_timestamp(0),
                    ));
                    total_actions.fetch_add(actions.len(), Ordering::Relaxed);
                }
            }));
        }

        for t in threads {
            t.join().unwrap();
        }

        // All registrations should have completed.
        assert_eq!(bus.handler_count(), n_registrars * n_ops);
        // Every firer starts only after each registrar has installed one
        // handler, so this assertion does not depend on thread scheduling.
        assert!(total_actions.load(Ordering::Relaxed) > 0);
    }

    /// Concurrent register + deregister: verifies handler_count remains
    /// consistent under mixed write contention.
    #[test]
    fn concurrent_register_deregister() {
        let bus = Arc::new(EventBus::new());
        let n_threads = 4;
        let n_ops = 500;
        let barrier = Arc::new(Barrier::new(n_threads));

        let threads: Vec<_> = (0..n_threads)
            .map(|_| {
                let bus = bus.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    let mut ids = Vec::new();
                    for _ in 0..n_ops {
                        let h: Arc<HandlerFn> = Arc::new(|_| vec![]);
                        let id = bus
                            .register(HandlerPriority::Native, None, h)
                            .expect("handler registration should succeed");
                        ids.push(id);
                    }
                    // Deregister all in reverse order.
                    for id in ids.into_iter().rev() {
                        bus.deregister(id);
                    }
                })
            })
            .collect();

        for t in threads {
            t.join().unwrap();
        }

        // All handlers should be deregistered.
        assert_eq!(bus.handler_count(), 0);
    }

    /// Simulates 60 Hz update-status pattern under concurrent fire from
    /// multiple producer threads (pane output, resize, user-var-changed).
    #[test]
    fn concurrent_multi_event_type_dispatch() {
        let bus = Arc::new(EventBus::new());

        // Register handlers for different event types.
        let status_count = Arc::new(AtomicUsize::new(0));
        let output_count = Arc::new(AtomicUsize::new(0));
        let resize_count = Arc::new(AtomicUsize::new(0));

        {
            let c = status_count.clone();
            let h: Arc<HandlerFn> = Arc::new(move |_| {
                c.fetch_add(1, Ordering::Relaxed);
                vec![]
            });
            bus.register(HandlerPriority::Native, Some(EventType::UpdateStatus), h)
                .expect("handler registration should succeed");
        }
        {
            let c = output_count.clone();
            let h: Arc<HandlerFn> = Arc::new(move |_| {
                c.fetch_add(1, Ordering::Relaxed);
                vec![]
            });
            bus.register(HandlerPriority::Native, Some(EventType::PaneOutput), h)
                .expect("handler registration should succeed");
        }
        {
            let c = resize_count.clone();
            let h: Arc<HandlerFn> = Arc::new(move |_| {
                c.fetch_add(1, Ordering::Relaxed);
                vec![]
            });
            bus.register(HandlerPriority::Native, Some(EventType::WindowResized), h)
                .expect("handler registration should succeed");
        }

        let n_fires = 1000;
        let barrier = Arc::new(Barrier::new(3));

        let bus1 = bus.clone();
        let b1 = barrier.clone();
        let t1 = std::thread::spawn(move || {
            b1.wait();
            for _ in 0..n_fires {
                bus1.fire(&Event::with_timestamp(
                    EventType::UpdateStatus,
                    EventPayload::Status { pane_id: 0 },
                    test_timestamp(0),
                ));
            }
        });

        let bus2 = bus.clone();
        let b2 = barrier.clone();
        let t2 = std::thread::spawn(move || {
            b2.wait();
            for _ in 0..n_fires {
                bus2.fire(&Event::with_timestamp(
                    EventType::PaneOutput,
                    EventPayload::PaneText {
                        pane_id: 1,
                        text: Arc::from("data"),
                    },
                    test_timestamp(0),
                ));
            }
        });

        let bus3 = bus.clone();
        let b3 = barrier.clone();
        let t3 = std::thread::spawn(move || {
            b3.wait();
            for _ in 0..n_fires {
                bus3.fire(&Event::with_timestamp(
                    EventType::WindowResized,
                    EventPayload::Resize {
                        window_id: 0,
                        rows: 24,
                        cols: 80,
                    },
                    test_timestamp(0),
                ));
            }
        });

        t1.join().unwrap();
        t2.join().unwrap();
        t3.join().unwrap();

        // Each event type should have been dispatched exactly n_fires times.
        assert_eq!(status_count.load(Ordering::Relaxed), n_fires);
        assert_eq!(output_count.load(Ordering::Relaxed), n_fires);
        assert_eq!(resize_count.load(Ordering::Relaxed), n_fires);
    }

    /// Stress test: sustained 10K events/second for 100ms.
    #[test]
    fn sustained_high_throughput() {
        let bus = Arc::new(EventBus::new());
        let call_count = Arc::new(AtomicUsize::new(0));

        // 3 native handlers for the status event.
        for _ in 0..3 {
            let c = call_count.clone();
            let h: Arc<HandlerFn> = Arc::new(move |_| {
                c.fetch_add(1, Ordering::Relaxed);
                vec![]
            });
            bus.register(HandlerPriority::Native, Some(EventType::UpdateStatus), h)
                .expect("handler registration should succeed");
        }

        let n_events = 10_000;
        let event = Event::with_timestamp(
            EventType::UpdateStatus,
            EventPayload::Status { pane_id: 0 },
            test_timestamp(0),
        );

        for _ in 0..n_events {
            bus.fire(&event);
        }

        // 3 handlers * 10,000 events = 30,000 calls.
        assert_eq!(call_count.load(Ordering::Relaxed), 3 * n_events);
    }
}
