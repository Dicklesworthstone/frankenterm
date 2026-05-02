use frankenterm_core::cx::Cx;
use frankenterm_core::input_priority::{
    InputPriorityClass, NegotiatedPriority, OsPriorityHint, Platform, PriorityOutcomeStats,
    record_priority_outcome,
};
use frankenterm_core::runtime_async::{self, mpsc};
use std::future::Future;

const DEFAULT_INPUT_QUEUE_CAPACITY: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputLoopConfig {
    pub platform: Platform,
    pub priority_class: InputPriorityClass,
    pub queue_capacity: usize,
}

impl InputLoopConfig {
    #[must_use]
    pub fn low_latency_for_current_platform() -> Self {
        Self {
            platform: current_platform(),
            priority_class: InputPriorityClass::LowLatency,
            queue_capacity: DEFAULT_INPUT_QUEUE_CAPACITY,
        }
    }

    #[must_use]
    pub fn bounded_capacity(mut self, queue_capacity: usize) -> Self {
        self.queue_capacity = queue_capacity.max(1);
        self
    }
}

impl Default for InputLoopConfig {
    fn default() -> Self {
        Self::low_latency_for_current_platform()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputLoopCommand {
    PtyBytes(Vec<u8>),
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriorityApplyResult {
    pub negotiated: NegotiatedPriority,
    pub applied: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputLoopReport {
    pub processed_events: u64,
    pub dropped_events: u64,
    pub priority_stats: PriorityOutcomeStats,
    pub priority_apply: PriorityApplyResult,
}

#[derive(Debug, thiserror::Error)]
pub enum InputLoopError {
    #[error("input loop send failed: receiver closed")]
    SendClosed,
    #[error("input loop writer failed: {0}")]
    Writer(String),
    #[error("input loop task failed: {0}")]
    Join(String),
}

pub struct InputLoopHandle {
    tx: mpsc::Sender<InputLoopCommand>,
    task: runtime_async::task::JoinHandle<Result<InputLoopReport, InputLoopError>>,
}

impl InputLoopHandle {
    pub async fn enqueue_pty_bytes(
        &self,
        cx: &Cx,
        bytes: impl Into<Vec<u8>>,
    ) -> Result<(), InputLoopError> {
        self.tx
            .send(cx, InputLoopCommand::PtyBytes(bytes.into()))
            .await
            .map_err(|_| InputLoopError::SendClosed)
    }

    pub async fn shutdown(self, cx: &Cx) -> Result<InputLoopReport, InputLoopError> {
        self.tx
            .send(cx, InputLoopCommand::Shutdown)
            .await
            .map_err(|_| InputLoopError::SendClosed)?;
        self.join().await
    }

    pub async fn join(self) -> Result<InputLoopReport, InputLoopError> {
        self.task
            .await
            .map_err(|err| InputLoopError::Join(err.to_string()))?
    }
}

pub fn spawn_latency_pinned_input_loop<W, Fut>(
    cx: Cx,
    config: InputLoopConfig,
    writer: W,
) -> InputLoopHandle
where
    W: FnMut(Vec<u8>) -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), InputLoopError>> + Send + 'static,
{
    spawn_latency_pinned_input_loop_with_priority_applier(
        cx,
        config,
        default_priority_applier,
        writer,
    )
}

pub fn spawn_latency_pinned_input_loop_with_priority_applier<A, W, Fut>(
    cx: Cx,
    config: InputLoopConfig,
    priority_applier: A,
    writer: W,
) -> InputLoopHandle
where
    A: FnOnce(OsPriorityHint) -> bool + Send + 'static,
    W: FnMut(Vec<u8>) -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), InputLoopError>> + Send + 'static,
{
    let capacity = config.queue_capacity.max(1);
    let (tx, rx) = mpsc::channel(capacity);
    let task = runtime_async::task::spawn(run_input_loop(cx, config, rx, priority_applier, writer));
    InputLoopHandle { tx, task }
}

async fn run_input_loop<A, W, Fut>(
    cx: Cx,
    config: InputLoopConfig,
    mut rx: mpsc::Receiver<InputLoopCommand>,
    priority_applier: A,
    mut writer: W,
) -> Result<InputLoopReport, InputLoopError>
where
    A: FnOnce(OsPriorityHint) -> bool,
    W: FnMut(Vec<u8>) -> Fut,
    Fut: Future<Output = Result<(), InputLoopError>>,
{
    let negotiated = frankenterm_core::input_priority::negotiate_priority(
        config.priority_class,
        config.platform,
    );
    let applied = negotiated.fallback_reason.is_none() && priority_applier(negotiated.hint);
    let mut priority_stats = PriorityOutcomeStats::default();
    record_priority_outcome(&mut priority_stats, negotiated, applied);

    let mut processed_events = 0u64;
    let dropped_events = 0u64;

    while let Ok(command) = rx.recv(&cx).await {
        match command {
            InputLoopCommand::PtyBytes(bytes) => {
                writer(bytes).await?;
                processed_events = processed_events.saturating_add(1);
            }
            InputLoopCommand::Shutdown => break,
        }
    }

    Ok(InputLoopReport {
        processed_events,
        dropped_events,
        priority_stats,
        priority_apply: PriorityApplyResult {
            negotiated,
            applied,
        },
    })
}

#[must_use]
pub fn current_platform() -> Platform {
    if cfg!(target_os = "linux") {
        Platform::Linux
    } else if cfg!(target_os = "macos") {
        Platform::MacOs
    } else if cfg!(target_os = "windows") {
        Platform::Windows
    } else {
        Platform::Other
    }
}

#[must_use]
pub fn default_priority_applier(_hint: OsPriorityHint) -> bool {
    false
}
