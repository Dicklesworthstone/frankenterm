//! Guardian-owned PTY and child lifetime state.

use crate::output::{
    GuardianOutputCompletionState, GuardianOutputPipeline, GuardianOutputSubmitError,
    GuardianPaneInputCompletionError, GuardianPaneInputJournal,
    GuardianPaneInputTransaction, GuardianPaneInputTransactionError,
    GuardianPaneOutputJournal, OUTPUT_RECORD_BYTES,
};
use mio::Waker;
use mio::unix::SourceFd;
use mio::{Interest, Registry, Token};
use mux::guardian_input_journal::{
    GuardianInputDisposition, catch_guardian_input_worker_panic,
};
use mux::guardian_protocol::{
    AuthenticatedGuardianRequest, GuardianEffectOutcome, GuardianEffectTransactionError,
    GuardianMuxLeaseRetirement, GuardianOperation, GuardianPaneState, GuardianProtocolError,
    GuardianProtocolState, GuardianRejectionCode, GuardianReply, GuardianResizePayload,
    GuardianResponseEnvelope, GuardianSignal, GuardianSpawnPayload, InputEffectState,
    GUARDIAN_MAX_PANES,
};
use portable_pty::{
    Child, ChildKiller, MasterPty, PollablePtyReader, native_pty_system,
};
use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::sync::Arc;
use std::sync::mpsc::{
    Receiver, SyncSender, TryRecvError, TrySendError, sync_channel,
};
use std::thread::{self, JoinHandle};
use uuid::Uuid;
use zeroize::{Zeroize as _, Zeroizing};

/// Bounded resources assigned to one guardian runtime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuardianRuntimeConfig {
    max_panes: usize,
    // These two bounds govern resident pre-sync plaintext, not cumulative
    // encrypted journal retention. The journal owns separate hard disk caps.
    max_output_bytes_per_pane: usize,
    max_total_output_bytes: usize,
    first_pty_token: usize,
}

impl GuardianRuntimeConfig {
    pub(crate) fn new(
        max_panes: usize,
        max_output_bytes_per_pane: usize,
        max_total_output_bytes: usize,
        first_pty_token: usize,
    ) -> Result<Self, GuardianProtocolError> {
        let Some(possible_output_bytes) = max_panes.checked_mul(max_output_bytes_per_pane) else {
            return Err(GuardianProtocolError::CapacityExhausted);
        };
        if max_panes == 0
            || max_panes > GUARDIAN_MAX_PANES
            || max_output_bytes_per_pane == 0
            || max_total_output_bytes == 0
            || max_total_output_bytes > possible_output_bytes
            || first_pty_token == 0
            || first_pty_token.checked_add(max_panes).is_none()
        {
            return Err(GuardianProtocolError::CapacityExhausted);
        }
        Ok(Self {
            max_panes,
            max_output_bytes_per_pane,
            max_total_output_bytes,
            first_pty_token,
        })
    }
}

/// Content-free operational counters for readiness and child polling faults.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GuardianRuntimeCounters {
    pub input_activation_rejections: u64,
    pub input_transactions_submitted: u64,
    pub input_transactions_completed: u64,
    pub input_known_not_applied: u64,
    pub input_durable_prefixes: u64,
    pub input_retryable_capacity_closes: u64,
    pub input_worker_disconnects: u64,
    pub input_worker_panics: u64,
    pub pty_bytes_drained: u64,
    pub pty_bytes_durably_committed: u64,
    pub pty_records_durably_committed: u64,
    pub pty_read_failures: u64,
    pub output_commit_failures: u64,
    pub output_deregister_failures: u64,
    pub output_rearm_failures: u64,
    pub output_worker_disconnects: u64,
    pub output_segment_exhaustions: u64,
    pub child_poll_failures: u64,
    pub protocol_transition_failures: u64,
}

struct RuntimePaneOutput {
    journal: GuardianPaneOutputJournal,
    pending_plaintext: Option<Zeroizing<Vec<u8>>>,
    in_flight_bytes: usize,
    expected_sequence: Option<u64>,
    durable_plaintext_bytes: u64,
    remaining_record_capacity: u64,
    waiting_for_slot: bool,
    failed: bool,
}

impl RuntimePaneOutput {
    fn new(journal: GuardianPaneOutputJournal) -> Self {
        Self {
            expected_sequence: journal.initial_next_sequence(),
            durable_plaintext_bytes: journal.initial_cumulative_plaintext_bytes(),
            remaining_record_capacity: journal.initial_remaining_records(),
            journal,
            pending_plaintext: None,
            in_flight_bytes: 0,
            waiting_for_slot: false,
            failed: false,
        }
    }

    fn is_quiescent(&self) -> bool {
        self.pending_plaintext.is_none() && self.in_flight_bytes == 0 && !self.failed
    }
}

struct RuntimePane {
    _master: Box<dyn MasterPty>,
    writer: Option<Box<dyn Write + Send>>,
    input_journal: Option<GuardianPaneInputJournal>,
    reader: Box<dyn PollablePtyReader>,
    reader_registered: bool,
    child: Box<dyn Child + Send + Sync>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    output: RuntimePaneOutput,
    pty_eof_observed: bool,
    exit_observed: bool,
    token: Token,
}

#[derive(Default)]
struct OutputRearmCursor {
    last_serviced: Option<Uuid>,
}

impl OutputRearmCursor {
    fn select(
        &mut self,
        candidates: impl IntoIterator<Item = Uuid>,
    ) -> Option<Uuid> {
        let selected = round_robin_successor(self.last_serviced, candidates)?;
        self.last_serviced = Some(selected);
        Some(selected)
    }
}

const INPUT_WORKER_QUEUE_CAPACITY: usize = 1;

/// Exact transport authority retained across one delayed input transaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GuardianInputRoute {
    pub(crate) connection_token: Token,
    pub(crate) connection_generation: u64,
    pub(crate) request_id: Uuid,
    pub(crate) effect_id: Uuid,
}

impl GuardianInputRoute {
    pub(crate) fn new(
        connection_token: Token,
        connection_generation: u64,
        request_id: Uuid,
        effect_id: Uuid,
    ) -> Option<Self> {
        (connection_generation != 0
            && !request_id.is_nil()
            && !effect_id.is_nil())
        .then_some(Self {
            connection_token,
            connection_generation,
            request_id,
            effect_id,
        })
    }
}

pub(crate) enum GuardianInputSubmission {
    Pending,
    Respond(GuardianResponseEnvelope),
    CloseRetryably,
}

pub(crate) struct GuardianRuntimeInputCompletion {
    pub(crate) route: GuardianInputRoute,
    pub(crate) response: Option<GuardianResponseEnvelope>,
}

pub(crate) enum GuardianRuntimeInputCompletionState {
    Ready(GuardianRuntimeInputCompletion),
    Empty,
    Disconnected,
}

/// Owned authenticated request whose plaintext is wiped on every exit path.
///
/// The worker still wipes immediately after the PTY write, but correctness no
/// longer depends on reaching that manual fast path: busy rejection, queue
/// failure, authority restoration, panic recovery, and ordinary drop all pass
/// through this guard.
struct OwnedInputRequest {
    request: AuthenticatedGuardianRequest,
    #[cfg(test)]
    wipe_probe: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl OwnedInputRequest {
    fn new(request: AuthenticatedGuardianRequest) -> Self {
        Self {
            request,
            #[cfg(test)]
            wipe_probe: None,
        }
    }

    #[cfg(test)]
    fn set_wipe_probe(&mut self, probe: Option<Arc<std::sync::atomic::AtomicBool>>) {
        self.wipe_probe = probe;
    }
}

impl std::ops::Deref for OwnedInputRequest {
    type Target = AuthenticatedGuardianRequest;

    fn deref(&self) -> &Self::Target {
        &self.request
    }
}

impl std::ops::DerefMut for OwnedInputRequest {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.request
    }
}

impl Drop for OwnedInputRequest {
    fn drop(&mut self) {
        self.request.zeroize_payload();
        #[cfg(test)]
        if let Some(probe) = self.wipe_probe.as_ref() {
            probe.store(
                self.request.payload().is_empty(),
                std::sync::atomic::Ordering::SeqCst,
            );
        }
    }
}

struct InputJob {
    route: GuardianInputRoute,
    pane_id: Uuid,
    protocol: GuardianProtocolState,
    writer: Box<dyn Write + Send>,
    journal: GuardianPaneInputJournal,
    request: OwnedInputRequest,
}

struct InputJobExecution {
    response: Option<GuardianResponseEnvelope>,
    disposition: Option<GuardianInputDisposition>,
}

struct InputWorkerCompletion {
    route: GuardianInputRoute,
    pane_id: Uuid,
    protocol: GuardianProtocolState,
    writer: Box<dyn Write + Send>,
    journal: GuardianPaneInputJournal,
    response: Option<GuardianResponseEnvelope>,
    disposition: Option<GuardianInputDisposition>,
    worker_panicked: bool,
}

enum InputSubmitError {
    Saturated(InputJob),
    Unavailable(InputJob),
}

struct GuardianInputPipeline {
    jobs: Option<SyncSender<InputJob>>,
    completions: Option<Receiver<InputWorkerCompletion>>,
    worker: Option<JoinHandle<()>>,
    _completion_waker: Arc<Waker>,
}

impl GuardianInputPipeline {
    fn new(completion_waker: Arc<Waker>) -> Result<Self, GuardianProtocolError> {
        let (jobs, job_receiver) = sync_channel(INPUT_WORKER_QUEUE_CAPACITY);
        let (completion_sender, completions) = sync_channel(INPUT_WORKER_QUEUE_CAPACITY);
        let worker_waker = Arc::clone(&completion_waker);
        let worker = thread::Builder::new()
            .name("ft-guardian-input".to_string())
            .spawn(move || input_worker(job_receiver, completion_sender, worker_waker))
            .map_err(|_| {
                GuardianProtocolError::StateInvariantViolation(
                    "guardian-input-worker-spawn",
                )
            })?;
        Ok(Self {
            jobs: Some(jobs),
            completions: Some(completions),
            worker: Some(worker),
            _completion_waker: completion_waker,
        })
    }

    fn try_submit(&self, job: InputJob) -> Result<(), InputSubmitError> {
        let Some(jobs) = self.jobs.as_ref() else {
            return Err(InputSubmitError::Unavailable(job));
        };
        jobs.try_send(job).map_err(|error| match error {
            TrySendError::Full(job) => InputSubmitError::Saturated(job),
            TrySendError::Disconnected(job) => InputSubmitError::Unavailable(job),
        })
    }

    fn try_completion(&self) -> GuardianRuntimeInputCompletionStateInternal {
        let Some(completions) = self.completions.as_ref() else {
            return GuardianRuntimeInputCompletionStateInternal::Disconnected;
        };
        match completions.try_recv() {
            Ok(completion) => GuardianRuntimeInputCompletionStateInternal::Ready(completion),
            Err(TryRecvError::Empty) => GuardianRuntimeInputCompletionStateInternal::Empty,
            Err(TryRecvError::Disconnected) => {
                GuardianRuntimeInputCompletionStateInternal::Disconnected
            }
        }
    }
}

impl Drop for GuardianInputPipeline {
    fn drop(&mut self) {
        drop(self.completions.take());
        drop(self.jobs.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

enum GuardianRuntimeInputCompletionStateInternal {
    Ready(InputWorkerCompletion),
    Empty,
    Disconnected,
}

fn input_worker(
    jobs: Receiver<InputJob>,
    completions: SyncSender<InputWorkerCompletion>,
    completion_waker: Arc<Waker>,
) {
    while let Ok(mut job) = jobs.recv() {
        let execution = catch_guardian_input_worker_panic(|| execute_input_job(&mut job));
        // The catch boundary retains the outer job even if a writer or journal
        // panics.  Wipe plaintext before publishing any content-free result.
        job.request.zeroize_payload();
        let (response, disposition, worker_panicked) = match execution {
            Ok(execution) => (execution.response, execution.disposition, false),
            Err(_) => (None, None, true),
        };
        let completion = InputWorkerCompletion {
            route: job.route,
            pane_id: job.pane_id,
            protocol: job.protocol,
            writer: job.writer,
            journal: job.journal,
            response,
            disposition,
            worker_panicked,
        };
        if completions.send(completion).is_err() {
            return;
        }
        let _ = completion_waker.wake();
    }
}

fn execute_input_job(job: &mut InputJob) -> InputJobExecution {
    let transaction = job
        .journal
        .begin_transaction(&mut job.protocol, &job.request);
    match transaction {
        Ok(GuardianPaneInputTransaction::Reconciled(reply)) => {
            input_reply_execution(&job.request, reply)
        }
        Ok(GuardianPaneInputTransaction::WriteAuthorized {
            accepted_reply: _,
            permit,
        }) => {
            let outcome = permit.write_once(job.writer.as_mut(), job.request.payload());
            // The original authenticated byte length remains available for
            // terminal correlation, so plaintext can die before terminal fsync.
            job.request.zeroize_payload();
            match job
                .journal
                .complete_write(&mut job.protocol, outcome)
            {
                Ok(reply) => input_reply_execution(&job.request, reply),
                Err(
                    GuardianPaneInputCompletionError::DispositionIndeterminate
                    | GuardianPaneInputCompletionError::Journal
                    | GuardianPaneInputCompletionError::Authority
                    | GuardianPaneInputCompletionError::Protocol,
                ) => InputJobExecution {
                    response: None,
                    disposition: None,
                },
            }
        }
        Err(GuardianPaneInputTransactionError::Protocol(error)) => InputJobExecution {
            response: Some(GuardianResponseEnvelope::rejection(
                &job.request,
                GuardianRejectionCode::from_protocol_error(&error),
            )),
            disposition: None,
        },
        Err(
            GuardianPaneInputTransactionError::JournalBeforeWrite
            | GuardianPaneInputTransactionError::AuthorityBeforeWrite,
        ) => InputJobExecution {
            // No durable input disposition exists, so even a definitely
            // pre-write infrastructure failure is not represented as a
            // terminal effect result. Close retryably and let only protocol
            // precondition errors use ordinary rejection codes.
            response: None,
            disposition: None,
        },
        Err(
            GuardianPaneInputTransactionError::OutcomeIndeterminate
            | GuardianPaneInputTransactionError::AcceptedJournalUnavailable(_)
            | GuardianPaneInputTransactionError::AcceptedAuthorityUnavailable
            | GuardianPaneInputTransactionError::AcceptedProtocolUnavailable(_),
        ) => InputJobExecution {
            response: None,
            disposition: None,
        },
    }
}

fn input_reply_execution(
    request: &AuthenticatedGuardianRequest,
    reply: GuardianReply,
) -> InputJobExecution {
    let disposition = match &reply {
        GuardianReply::InputReceipt { state, .. } => match *state {
            InputEffectState::AcceptedNotDurable => {
                Some(GuardianInputDisposition::AcceptedNotDurable)
            }
            InputEffectState::DurableFull => Some(GuardianInputDisposition::DurableFull),
            InputEffectState::DurablePrefix { applied_bytes } => {
                Some(GuardianInputDisposition::DurablePrefix { applied_bytes })
            }
            InputEffectState::KnownNotApplied => {
                Some(GuardianInputDisposition::KnownNotApplied)
            }
            InputEffectState::NotSeen | InputEffectState::DispositionUnavailable => None,
        },
        _ => None,
    };
    let response = match disposition {
        Some(GuardianInputDisposition::KnownNotApplied) => {
            Some(GuardianResponseEnvelope::rejection(
                request,
                GuardianRejectionCode::InputKnownNotApplied,
            ))
        }
        Some(
            GuardianInputDisposition::DurableFull
            | GuardianInputDisposition::DurablePrefix { .. },
        ) => GuardianResponseEnvelope::reply(request, &reply).ok(),
        Some(
            GuardianInputDisposition::Intent
            | GuardianInputDisposition::AcceptedNotDurable,
        )
        | None => None,
    };
    InputJobExecution {
        response,
        disposition,
    }
}

/// One process-local owner of native PTYs, child handles, and fencing state.
pub struct GuardianRuntime {
    incarnation: Uuid,
    protocol: Option<GuardianProtocolState>,
    registry: Registry,
    config: GuardianRuntimeConfig,
    panes: HashMap<Uuid, RuntimePane>,
    pty_tokens: HashMap<Token, Uuid>,
    next_pty_token: usize,
    buffered_output_bytes: usize,
    output_pipeline: GuardianOutputPipeline,
    input_pipeline: GuardianInputPipeline,
    input_pipeline_failed: bool,
    // This slot is reachable only if the pane map violates the invariant that
    // a worker-owned pane cannot retire while the sole protocol authority is
    // in flight.  Retain the descriptor-pinned WAL and writer even then: an
    // invariant failure must quarantine input, never silently drop its only
    // recovery authority.
    orphaned_input_authority:
        Option<(Box<dyn Write + Send>, GuardianPaneInputJournal)>,
    pending_child_exits: Vec<(Uuid, i32)>,
    output_pipeline_failed: bool,
    output_rearm_cursor: OutputRearmCursor,
    indeterminate_effect: bool,
    counters: GuardianRuntimeCounters,
    #[cfg(test)]
    input_request_wipe_probe: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl GuardianRuntime {
    pub(crate) fn new(
        registry: Registry,
        config: GuardianRuntimeConfig,
        incarnation: Uuid,
        output_pipeline: GuardianOutputPipeline,
        completion_waker: Arc<Waker>,
    ) -> Result<Self, GuardianProtocolError> {
        let mut pending_child_exits = Vec::new();
        pending_child_exits
            .try_reserve_exact(config.max_panes)
            .map_err(|_| GuardianProtocolError::CapacityExhausted)?;
        let input_pipeline = GuardianInputPipeline::new(completion_waker)?;
        Ok(Self {
            incarnation,
            protocol: Some(GuardianProtocolState::new(incarnation)?),
            registry,
            config,
            panes: HashMap::new(),
            pty_tokens: HashMap::new(),
            next_pty_token: config.first_pty_token,
            buffered_output_bytes: 0,
            output_pipeline,
            input_pipeline,
            input_pipeline_failed: false,
            orphaned_input_authority: None,
            pending_child_exits,
            output_pipeline_failed: false,
            output_rearm_cursor: OutputRearmCursor::default(),
            indeterminate_effect: false,
            counters: GuardianRuntimeCounters::default(),
            #[cfg(test)]
            input_request_wipe_probe: None,
        })
    }

    #[must_use]
    pub const fn incarnation(&self) -> Uuid {
        self.incarnation
    }

    #[must_use]
    pub fn pane_count(&self) -> usize {
        let retained_authority = usize::from(self.orphaned_input_authority.is_some());
        effective_pane_occupancy(
            self.panes.len().saturating_add(retained_authority),
            self.indeterminate_effect,
        )
    }

    #[must_use]
    pub const fn buffered_output_bytes(&self) -> usize {
        self.buffered_output_bytes
    }

    #[must_use]
    pub const fn counters(&self) -> GuardianRuntimeCounters {
        self.counters
    }

    #[must_use]
    pub fn owns_pty_token(&self, token: Token) -> bool {
        self.pty_tokens.contains_key(&token)
    }

    /// Dispatch one already authenticated request.
    ///
    /// Input is submitted through [`Self::submit_input`] so its request and
    /// connection survive the off-loop durable transaction. Checkpoint and
    /// output replay remain fail-closed until their publishers exist.
    pub fn dispatch(
        &mut self,
        request: &AuthenticatedGuardianRequest,
    ) -> Option<GuardianResponseEnvelope> {
        // Hello is the one protocol-independent observation: the immutable
        // incarnation is cached specifically so a mux can authenticate while
        // the sole mutable protocol authority is worker-owned. Every other
        // operation closes without a response as an explicit retryable busy
        // fence; no caller is told that an operation failed terminally while
        // its preflight cannot be evaluated.
        if self.protocol.is_none() {
            if request.header().operation == GuardianOperation::Hello
                && request.payload().is_empty()
            {
                return GuardianResponseEnvelope::reply(
                    request,
                    &GuardianReply::Hello {
                        guardian_incarnation: self.incarnation,
                    },
                )
                .ok();
            }
            return None;
        }
        if self.indeterminate_effect
            && !operation_allowed_during_effect_quarantine(request.header().operation)
        {
            // Only an exact retained indeterminate identity may receive a
            // typed diagnostic receipt. Every new/conflicting mutation closes
            // without a response: a terminal rejection would falsely imply
            // that the earlier external effect definitely did not apply.
            return self
                .protocol
                .as_ref()?
                .indeterminate_effect_reply(request)
                .ok()
                .flatten()
                .and_then(|reply| GuardianResponseEnvelope::reply(request, &reply).ok());
        }
        let effect_was_indeterminate = self.indeterminate_effect;
        let result = match request.header().operation {
            GuardianOperation::Input => {
                // A borrowed request cannot cross the worker boundary. The
                // transport must route owned Input through `submit_input`.
                self.counters.input_activation_rejections = self
                    .counters
                    .input_activation_rejections
                    .saturating_add(1);
                return None;
            }
            GuardianOperation::Checkpoint
            | GuardianOperation::Replay
            | GuardianOperation::GuardedStop => Err(GuardianRejectionCode::InvalidRequest),
            GuardianOperation::Hello => {
                if request.payload().is_empty() {
                    self.apply_observation(request)
                } else {
                    Err(GuardianRejectionCode::InvalidRequest)
                }
            }
            GuardianOperation::Census | GuardianOperation::QueryInputEffect => {
                self.apply_observation(request)
            }
            GuardianOperation::Attach => {
                if request.payload().is_empty() {
                    self.apply_observation(request)
                } else {
                    Err(GuardianRejectionCode::InvalidRequest)
                }
            }
            GuardianOperation::Spawn => self.apply_spawn(request),
            GuardianOperation::Claim | GuardianOperation::RetireLease => {
                if request.payload().is_empty() {
                    self.apply_metadata_effect(request)
                } else {
                    Err(GuardianRejectionCode::InvalidRequest)
                }
            }
            GuardianOperation::Resize => self.apply_resize(request),
            GuardianOperation::Signal => self.apply_signal(request),
            GuardianOperation::Close => {
                if request.payload().is_empty() {
                    self.apply_close(request)
                } else {
                    Err(GuardianRejectionCode::InvalidRequest)
                }
            }
        };

        match result {
            // A post-commit reply construction failure must close the
            // connection without a false terminal rejection. The exact
            // authenticated retry can then recover the retained receipt.
            Ok(reply) => GuardianResponseEnvelope::reply(request, &reply).ok(),
            Err(_) if newly_indeterminate_effect(
                effect_was_indeterminate,
                self.indeterminate_effect,
            ) => None,
            Err(code) => Some(GuardianResponseEnvelope::rejection(request, code)),
        }
    }

    /// Transfer the one global protocol authority and target pane's input
    /// handles to the fixed worker. Saturation closes retryably without a
    /// response; it never fabricates a terminal rejection and never writes.
    pub(crate) fn submit_input(
        &mut self,
        request: AuthenticatedGuardianRequest,
        route: GuardianInputRoute,
    ) -> GuardianInputSubmission {
        if request.header().operation != GuardianOperation::Input
            || request.header().request_id != route.request_id
            || request.header().effect_id != Some(route.effect_id)
        {
            return GuardianInputSubmission::Respond(
                GuardianResponseEnvelope::rejection(
                    &request,
                    GuardianRejectionCode::InvalidRequest,
                ),
            );
        }
        if self.input_pipeline_failed || self.protocol.is_none() {
            self.counters.input_retryable_capacity_closes = self
                .counters
                .input_retryable_capacity_closes
                .saturating_add(1);
            return GuardianInputSubmission::CloseRetryably;
        }
        if self.indeterminate_effect {
            let response = self
                .protocol
                .as_ref()
                .and_then(|protocol| protocol.indeterminate_effect_reply(&request).ok())
                .flatten()
                .and_then(|reply| GuardianResponseEnvelope::reply(&request, &reply).ok());
            return match response {
                Some(response) => GuardianInputSubmission::Respond(response),
                None => GuardianInputSubmission::CloseRetryably,
            };
        }
        let Some(pane_id) = request.header().pane_id else {
            return GuardianInputSubmission::Respond(
                GuardianResponseEnvelope::rejection(
                    &request,
                    GuardianRejectionCode::InvalidRequest,
                ),
            );
        };
        let Some(pane) = self.panes.get_mut(&pane_id) else {
            return GuardianInputSubmission::Respond(
                GuardianResponseEnvelope::rejection(
                    &request,
                    GuardianRejectionCode::PaneNotFound,
                ),
            );
        };
        let Some(writer) = pane.writer.take() else {
            self.counters.input_retryable_capacity_closes = self
                .counters
                .input_retryable_capacity_closes
                .saturating_add(1);
            return GuardianInputSubmission::CloseRetryably;
        };
        let Some(journal) = pane.input_journal.take() else {
            pane.writer = Some(writer);
            self.counters.input_retryable_capacity_closes = self
                .counters
                .input_retryable_capacity_closes
                .saturating_add(1);
            return GuardianInputSubmission::CloseRetryably;
        };
        let Some(protocol) = self.protocol.take() else {
            pane.writer = Some(writer);
            pane.input_journal = Some(journal);
            self.counters.input_retryable_capacity_closes = self
                .counters
                .input_retryable_capacity_closes
                .saturating_add(1);
            return GuardianInputSubmission::CloseRetryably;
        };
        let job = InputJob {
            route,
            pane_id,
            protocol,
            writer,
            journal,
            request,
        };
        match self.input_pipeline.try_submit(job) {
            Ok(()) => {
                self.counters.input_transactions_submitted = self
                    .counters
                    .input_transactions_submitted
                    .saturating_add(1);
                GuardianInputSubmission::Pending
            }
            Err(InputSubmitError::Saturated(job)) => {
                self.restore_unsent_input_job(job);
                self.counters.input_retryable_capacity_closes = self
                    .counters
                    .input_retryable_capacity_closes
                    .saturating_add(1);
                GuardianInputSubmission::CloseRetryably
            }
            Err(InputSubmitError::Unavailable(job)) => {
                self.restore_unsent_input_job(job);
                self.input_pipeline_failed = true;
                self.counters.input_worker_disconnects = self
                    .counters
                    .input_worker_disconnects
                    .saturating_add(1);
                GuardianInputSubmission::CloseRetryably
            }
        }
    }

    fn restore_unsent_input_job(&mut self, job: InputJob) {
        debug_assert!(self.protocol.is_none());
        self.protocol = Some(job.protocol);
        if !self.restore_pane_input_authority(job.pane_id, job.writer, job.journal) {
            self.counters.protocol_transition_failures = self
                .counters
                .protocol_transition_failures
                .saturating_add(1);
        }
    }

    fn restore_pane_input_authority(
        &mut self,
        pane_id: Uuid,
        writer: Box<dyn Write + Send>,
        journal: GuardianPaneInputJournal,
    ) -> bool {
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            if pane.writer.is_none() && pane.input_journal.is_none() {
                pane.writer = Some(writer);
                pane.input_journal = Some(journal);
                return true;
            }
        }
        self.indeterminate_effect = true;
        // Only one job can exist because it owns the sole protocol state;
        // therefore a second orphan is structurally unreachable.
        debug_assert!(self.orphaned_input_authority.is_none());
        self.orphaned_input_authority = Some((writer, journal));
        false
    }

    /// Restore worker-owned authorities, replay observations accumulated while
    /// the protocol was absent, and yield one exact transport completion.
    pub(crate) fn try_input_completion(&mut self) -> GuardianRuntimeInputCompletionState {
        match self.input_pipeline.try_completion() {
            GuardianRuntimeInputCompletionStateInternal::Ready(completion) => {
                debug_assert!(self.protocol.is_none());
                self.protocol = Some(completion.protocol);
                if !self.restore_pane_input_authority(
                    completion.pane_id,
                    completion.writer,
                    completion.journal,
                ) {
                    self.counters.protocol_transition_failures = self
                        .counters
                        .protocol_transition_failures
                        .saturating_add(1);
                    self.replay_deferred_protocol_observations();
                    return GuardianRuntimeInputCompletionState::Ready(
                        GuardianRuntimeInputCompletion {
                            route: completion.route,
                            response: None,
                        },
                    );
                }
                self.counters.input_transactions_completed = self
                    .counters
                    .input_transactions_completed
                    .saturating_add(1);
                if completion.worker_panicked {
                    // The outer job bundle survived, but a panic can leave an
                    // inner protocol/journal/writer operation at an unknown
                    // semantic point. Retaining AcceptedNotDurable prevents a
                    // second write; the global quarantine fences unrelated
                    // mutations from trusting possibly half-updated state.
                    self.indeterminate_effect = true;
                    self.counters.input_worker_panics = self
                        .counters
                        .input_worker_panics
                        .saturating_add(1);
                }
                match completion.disposition {
                    Some(GuardianInputDisposition::KnownNotApplied) => {
                        self.counters.input_known_not_applied = self
                            .counters
                            .input_known_not_applied
                            .saturating_add(1);
                    }
                    Some(GuardianInputDisposition::DurablePrefix { .. }) => {
                        self.counters.input_durable_prefixes = self
                            .counters
                            .input_durable_prefixes
                            .saturating_add(1);
                    }
                    Some(
                        GuardianInputDisposition::Intent
                        | GuardianInputDisposition::AcceptedNotDurable
                        | GuardianInputDisposition::DurableFull,
                    )
                    | None => {}
                }
                self.replay_deferred_protocol_observations();
                GuardianRuntimeInputCompletionState::Ready(
                    GuardianRuntimeInputCompletion {
                        route: completion.route,
                        response: completion.response,
                    },
                )
            }
            GuardianRuntimeInputCompletionStateInternal::Empty => {
                GuardianRuntimeInputCompletionState::Empty
            }
            GuardianRuntimeInputCompletionStateInternal::Disconnected => {
                if !self.input_pipeline_failed {
                    self.input_pipeline_failed = true;
                    self.counters.input_worker_disconnects = self
                        .counters
                        .input_worker_disconnects
                        .saturating_add(1);
                }
                GuardianRuntimeInputCompletionState::Disconnected
            }
        }
    }

    fn replay_deferred_protocol_observations(&mut self) {
        let Some(protocol) = self.protocol.as_mut() else {
            return;
        };
        while let Some((pane_id, exit_status)) = self.pending_child_exits.pop() {
            if !matches!(
                protocol.mark_exited(pane_id, exit_status),
                Ok(()) | Err(GuardianProtocolError::PaneTerminal)
            ) {
                self.indeterminate_effect = true;
                self.counters.protocol_transition_failures = self
                    .counters
                    .protocol_transition_failures
                    .saturating_add(1);
            }
        }
        while let Some(retirement) = self.pending_mux_retirements.pop() {
            if protocol
                .retire_disconnected_mux_leases(retirement.mux_incarnation)
                .is_err()
            {
                self.indeterminate_effect = true;
                self.counters.protocol_transition_failures = self
                    .counters
                    .protocol_transition_failures
                    .saturating_add(1);
            }
        }
        self.release_silent_closed_panes();
    }

    pub fn retire_disconnected_mux(
        &mut self,
        mux_incarnation: Uuid,
        disconnected_connection_generation: u64,
    ) -> Result<GuardianMuxLeaseRetirement, GuardianProtocolError> {
        if mux_incarnation.is_nil() {
            return Err(GuardianProtocolError::ZeroIdentity("mux incarnation"));
        }
        if disconnected_connection_generation == 0 {
            return Err(GuardianProtocolError::StateInvariantViolation(
                "guardian disconnected mux connection generation is zero",
            ));
        }
        if self.indeterminate_effect {
            return Err(GuardianProtocolError::StateInvariantViolation(
                "guardian external effects are quarantined after an indeterminate outcome",
            ));
        }
        let Some(protocol) = self.protocol.as_mut() else {
            if let Some(retirement) = self
                .pending_mux_retirements
                .iter_mut()
                .find(|retirement| retirement.mux_incarnation == mux_incarnation)
            {
                retirement.disconnected_connection_generation = retirement
                    .disconnected_connection_generation
                    .max(disconnected_connection_generation);
                return Ok(GuardianMuxLeaseRetirement::default());
            }
            if self.pending_mux_retirements.len()
                >= self.config.max_pending_mux_retirements
            {
                self.indeterminate_effect = true;
                return Err(GuardianProtocolError::CapacityExhausted);
            }
            self.pending_mux_retirements.push(PendingMuxRetirement {
                mux_incarnation,
                disconnected_connection_generation,
            });
            return Ok(GuardianMuxLeaseRetirement::default());
        };
        protocol.retire_disconnected_mux_leases(mux_incarnation)
    }

    /// Cancel a deferred last-connection retirement after a newer connection
    /// for the same mux has completed an authenticated Hello.
    ///
    /// This is deliberately usable while the input worker owns the protocol:
    /// the vector is readiness-loop-owned transport observation state, not
    /// protocol authority.  A strict generation comparison makes stale Hello
    /// observations powerless against newer disconnects.
    pub(crate) fn observe_connected_mux(
        &mut self,
        mux_incarnation: Uuid,
        connection_generation: u64,
    ) -> Result<(), GuardianProtocolError> {
        if mux_incarnation.is_nil() {
            return Err(GuardianProtocolError::ZeroIdentity("mux incarnation"));
        }
        if connection_generation == 0 {
            return Err(GuardianProtocolError::StateInvariantViolation(
                "guardian connected mux connection generation is zero",
            ));
        }
        self.pending_mux_retirements.retain(|retirement| {
            retirement.mux_incarnation != mux_incarnation
                || retirement.disconnected_connection_generation >= connection_generation
        });
        Ok(())
    }

    /// Read one bounded PTY record, then pause readiness until its encrypted
    /// journal append has synchronized and the completion receipt is applied.
    pub fn handle_pty_ready(&mut self, token: Token) {
        let Some(pane_id) = self.pty_tokens.get(&token).copied() else {
            return;
        };
        let pipeline_has_capacity = !self.output_pipeline_failed
            && self.output_pipeline.available_slots() != 0;
        let read_count = {
            let Some(pane) = self.panes.get_mut(&pane_id) else {
                self.counters.protocol_transition_failures = self
                    .counters
                    .protocol_transition_failures
                    .saturating_add(1);
                return;
            };
            if pane.output.failed
                || pane.output.pending_plaintext.is_some()
                || pane.output.in_flight_bytes != 0
            {
                deregister_reader(&self.registry, pane, &mut self.counters);
                return;
            }
            if pane.output.expected_sequence.is_none() {
                pane.output.failed = true;
                self.counters.output_commit_failures = self
                    .counters
                    .output_commit_failures
                    .saturating_add(1);
                deregister_reader(&self.registry, pane, &mut self.counters);
                return;
            }
            if pane.output.remaining_record_capacity == 0 {
                pane.output.failed = true;
                self.counters.output_segment_exhaustions = self
                    .counters
                    .output_segment_exhaustions
                    .saturating_add(1);
                deregister_reader(&self.registry, pane, &mut self.counters);
                return;
            }
            if !pipeline_has_capacity {
                pane.output.waiting_for_slot = true;
                deregister_reader(&self.registry, pane, &mut self.counters);
                return;
            }
            let remaining = remaining_output_capacity(
                self.config,
                0,
                self.buffered_output_bytes,
            );
            if remaining == 0 {
                pane.output.waiting_for_slot = true;
                deregister_reader(&self.registry, pane, &mut self.counters);
                return;
            }

            let read_len = remaining.min(OUTPUT_RECORD_BYTES);
            let mut bytes = Zeroizing::new(Vec::new());
            if bytes.try_reserve_exact(read_len).is_err() {
                self.counters.pty_read_failures =
                    self.counters.pty_read_failures.saturating_add(1);
                pane.output.failed = true;
                deregister_reader(&self.registry, pane, &mut self.counters);
                return;
            }
            bytes.resize(read_len, 0);
            let count = loop {
                match pane.reader.read(bytes.as_mut_slice()) {
                    Ok(0) => {
                        pane.pty_eof_observed = true;
                        deregister_reader(&self.registry, pane, &mut self.counters);
                        return;
                    }
                    Ok(count) => break count,
                    Err(error) if error.kind() == ErrorKind::WouldBlock => return,
                    Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => {
                        self.counters.pty_read_failures =
                            self.counters.pty_read_failures.saturating_add(1);
                        pane.output.failed = true;
                        deregister_reader(&self.registry, pane, &mut self.counters);
                        return;
                    }
                }
            };
            debug_assert!(count <= read_len);
            bytes.truncate(count);
            let Some(next_total) = self.buffered_output_bytes.checked_add(count) else {
                self.counters.pty_read_failures =
                    self.counters.pty_read_failures.saturating_add(1);
                pane.output.failed = true;
                deregister_reader(&self.registry, pane, &mut self.counters);
                return;
            };
            if next_total > self.config.max_total_output_bytes {
                self.counters.pty_read_failures =
                    self.counters.pty_read_failures.saturating_add(1);
                pane.output.failed = true;
                deregister_reader(&self.registry, pane, &mut self.counters);
                return;
            }
            self.buffered_output_bytes = next_total;
            pane.output.pending_plaintext = Some(bytes);
            pane.output.waiting_for_slot = true;
            deregister_reader(&self.registry, pane, &mut self.counters);
            count
        };
        debug_assert!(read_count != 0);
        self.resume_output_flow();
    }

    /// Apply every content-free worker completion. The worker explicitly
    /// zeroizes and drops the plaintext allocation before publishing one of
    /// these completions, so reader rearming cannot precede plaintext disposal.
    pub fn handle_output_completions(&mut self) {
        loop {
            match self.output_pipeline.try_completion() {
                GuardianOutputCompletionState::Ready(completion) => {
                    let Some(pane) = self.panes.get_mut(&completion.pane_id) else {
                        self.counters.protocol_transition_failures = self
                            .counters
                            .protocol_transition_failures
                            .saturating_add(1);
                        continue;
                    };
                    if pane.output.in_flight_bytes != completion.payload_bytes {
                        pane.output.failed = true;
                        pane.output.waiting_for_slot = false;
                        self.counters.output_commit_failures = self
                            .counters
                            .output_commit_failures
                            .saturating_add(1);
                        continue;
                    }
                    let Some(remaining_buffered_bytes) = self
                        .buffered_output_bytes
                        .checked_sub(completion.payload_bytes)
                    else {
                        pane.output.failed = true;
                        pane.output.waiting_for_slot = false;
                        self.counters.output_commit_failures = self
                            .counters
                            .output_commit_failures
                            .saturating_add(1);
                        continue;
                    };
                    self.buffered_output_bytes = remaining_buffered_bytes;
                    pane.output.in_flight_bytes = 0;
                    let failure_was_already_sticky = pane.output.failed;
                    match completion.result {
                        Ok(receipt)
                            if pane.output.journal.receipt_is_current(receipt)
                                && Some(receipt.sequence()) == pane.output.expected_sequence
                                && usize::try_from(receipt.payload_bytes()).ok()
                                    == Some(completion.payload_bytes)
                                && pane
                                    .output
                                    .durable_plaintext_bytes
                                    .checked_add(u64::from(receipt.payload_bytes()))
                                    == Some(receipt.cumulative_plaintext_bytes()) =>
                        {
                            let Some(remaining_record_capacity) =
                                pane.output.remaining_record_capacity.checked_sub(1)
                            else {
                                pane.output.failed = true;
                                pane.output.waiting_for_slot = false;
                                self.counters.output_commit_failures = self
                                    .counters
                                    .output_commit_failures
                                    .saturating_add(1);
                                continue;
                            };
                            pane.output.durable_plaintext_bytes =
                                receipt.cumulative_plaintext_bytes();
                            pane.output.expected_sequence = receipt.sequence().checked_add(1);
                            pane.output.remaining_record_capacity = remaining_record_capacity;
                            let has_append_capacity = remaining_record_capacity != 0
                                && pane.output.journal.can_accept_min_record();
                            pane.output.waiting_for_slot = !failure_was_already_sticky
                                && pane.output.expected_sequence.is_some()
                                && has_append_capacity;
                            pane.output.failed = failure_was_already_sticky
                                || pane.output.expected_sequence.is_none()
                                || !has_append_capacity;
                            self.counters.pty_bytes_drained = self
                                .counters
                                .pty_bytes_drained
                                .saturating_add(u64::from(receipt.payload_bytes()));
                            self.counters.pty_bytes_durably_committed = self
                                .counters
                                .pty_bytes_durably_committed
                                .saturating_add(u64::from(receipt.payload_bytes()));
                            self.counters.pty_records_durably_committed = self
                                .counters
                                .pty_records_durably_committed
                                .saturating_add(1);
                            if !has_append_capacity {
                                self.counters.output_segment_exhaustions = self
                                    .counters
                                    .output_segment_exhaustions
                                    .saturating_add(1);
                            }
                        }
                        Ok(_) | Err(_) => {
                            pane.output.failed = true;
                            pane.output.waiting_for_slot = false;
                            self.counters.output_commit_failures = self
                                .counters
                                .output_commit_failures
                                .saturating_add(1);
                        }
                    }
                }
                GuardianOutputCompletionState::Empty => break,
                GuardianOutputCompletionState::Disconnected => {
                    if !self.output_pipeline_failed {
                        self.output_pipeline_failed = true;
                        self.counters.output_worker_disconnects = self
                            .counters
                            .output_worker_disconnects
                            .saturating_add(1);
                        for pane in self.panes.values_mut() {
                            if let Some(mut payload) = pane.output.pending_plaintext.take() {
                                let payload_bytes = payload.len();
                                payload.zeroize();
                                drop(payload);
                                if let Some(remaining) =
                                    self.buffered_output_bytes.checked_sub(payload_bytes)
                                {
                                    self.buffered_output_bytes = remaining;
                                } else {
                                    self.counters.protocol_transition_failures = self
                                        .counters
                                        .protocol_transition_failures
                                        .saturating_add(1);
                                }
                            }
                            pane.output.failed = true;
                            pane.output.waiting_for_slot = false;
                            deregister_reader(&self.registry, pane, &mut self.counters);
                        }
                    }
                    break;
                }
            }
        }
        self.resume_output_flow();
    }

    fn resume_output_flow(&mut self) {
        if self.output_pipeline_failed {
            return;
        }
        let Self {
            output_pipeline,
            registry,
            panes,
            buffered_output_bytes,
            counters,
            output_rearm_cursor,
            ..
        } = self;

        // Submit previously read plaintext before rearming any new producer.
        // Hash-map order is irrelevant to correctness: each pane has at most
        // one pending/in-flight record and its own journal sequence authority.
        for (pane_id, pane) in panes.iter_mut() {
            if output_pipeline.available_slots() == 0 {
                break;
            }
            // A deregistration failure is sticky, but bytes read before that
            // failure still must reach durable storage. Such a pane may drain
            // its one existing pending record; it can never be rearmed.
            if pane.output.in_flight_bytes != 0 {
                continue;
            }
            let Some(payload) = pane.output.pending_plaintext.take() else {
                continue;
            };
            let payload_bytes = payload.len();
            match output_pipeline.try_submit(*pane_id, pane.output.journal.clone(), payload) {
                Ok(()) => {
                    pane.output.in_flight_bytes = payload_bytes;
                    pane.output.waiting_for_slot = false;
                }
                Err(GuardianOutputSubmitError::Saturated(payload)) => {
                    pane.output.pending_plaintext = Some(payload);
                    pane.output.waiting_for_slot = true;
                    break;
                }
                Err(GuardianOutputSubmitError::Unavailable(mut payload)) => {
                    let payload_bytes = payload.len();
                    payload.zeroize();
                    drop(payload);
                    if let Some(remaining) = buffered_output_bytes.checked_sub(payload_bytes) {
                        *buffered_output_bytes = remaining;
                    } else {
                        counters.protocol_transition_failures = counters
                            .protocol_transition_failures
                            .saturating_add(1);
                    }
                    pane.output.failed = true;
                    pane.output.waiting_for_slot = false;
                    counters.output_commit_failures =
                        counters.output_commit_failures.saturating_add(1);
                }
            }
        }

        let mut available_slots = output_pipeline.available_slots();
        if available_slots == 0 {
            return;
        }
        // A single UUID cursor gives deterministic round-robin service without
        // an unbounded/stale-ID queue. If its pane was removed, ordinary UUID
        // ordering advances to the next live candidate and then wraps.
        loop {
            if available_slots == 0 {
                break;
            }
            let Some(pane_id) = output_rearm_cursor.select(
                panes.iter().filter_map(|(pane_id, pane)| {
                    pane_is_rearm_candidate(pane).then_some(*pane_id)
                }),
            ) else {
                break;
            };
            let Some(pane) = panes.get_mut(&pane_id) else {
                counters.protocol_transition_failures = counters
                    .protocol_transition_failures
                    .saturating_add(1);
                continue;
            };
            if register_reader(registry, pane) {
                pane.output.waiting_for_slot = false;
                available_slots -= 1;
            } else {
                pane.output.failed = true;
                pane.output.waiting_for_slot = false;
                counters.output_rearm_failures =
                    counters.output_rearm_failures.saturating_add(1);
            }
        }
    }

    /// Poll every child once; no pane receives a waiter thread.
    pub fn reap_children_once(&mut self) {
        let Self {
            protocol,
            panes,
            pending_child_exits,
            indeterminate_effect,
            config,
            counters,
            ..
        } = self;
        for (pane_id, pane) in panes {
            if pane.exit_observed {
                continue;
            }
            match pane.child.try_wait() {
                Ok(Some(status)) => {
                    let exit_status = i32::try_from(status.exit_code()).unwrap_or(i32::MAX);
                    let recorded = if let Some(protocol) = protocol.as_mut() {
                        match protocol.mark_exited(*pane_id, exit_status) {
                            Ok(()) | Err(GuardianProtocolError::PaneTerminal) => true,
                            Err(_) => {
                                counters.protocol_transition_failures = counters
                                    .protocol_transition_failures
                                    .saturating_add(1);
                                false
                            }
                        }
                    } else if pending_child_exits.len() < config.max_panes {
                        pending_child_exits.push((*pane_id, exit_status));
                        true
                    } else {
                        // Never drop an exit observation under bounded-memory
                        // pressure. Quarantine all later mutations instead.
                        *indeterminate_effect = true;
                        counters.protocol_transition_failures = counters
                            .protocol_transition_failures
                            .saturating_add(1);
                        false
                    };
                    if recorded {
                        pane.exit_observed = true;
                    }
                }
                Ok(None) => {}
                Err(_) => {
                    counters.child_poll_failures =
                        counters.child_poll_failures.saturating_add(1);
                }
            }
        }
        self.release_silent_closed_panes();
    }

    /// Release live OS resources only when the pane is terminal, its child has
    /// exited, the PTY was drained through EOF, and no unjournaled transcript
    /// exists. Buffered terminal output or an ambiguous read boundary remains
    /// owned and therefore continues to block guarded shutdown.
    fn release_silent_closed_panes(&mut self) {
        let Self {
            protocol,
            registry,
            panes,
            pty_tokens,
            counters,
            ..
        } = self;
        let Some(protocol) = protocol.as_ref() else {
            return;
        };
        panes.retain(|pane_id, pane| {
            let release = terminal_resources_releasable(
                pane.exit_observed,
                pane.pty_eof_observed,
                pane.output.is_quiescent(),
                matches!(
                    protocol.pane_state(*pane_id),
                    Some(GuardianPaneState::ClosedTerminal { .. })
                ),
            );
            if release {
                deregister_reader(registry, pane, counters);
                pty_tokens.remove(&pane.token);
            }
            !release
        });
    }

    fn apply_observation(
        &mut self,
        request: &AuthenticatedGuardianRequest,
    ) -> Result<GuardianReply, GuardianRejectionCode> {
        self.protocol
            .as_mut()
            .ok_or(GuardianRejectionCode::InternalInvariant)?
            .apply_observation(request)
            .map_err(|error| GuardianRejectionCode::from_protocol_error(&error))
    }

    fn apply_spawn(
        &mut self,
        request: &AuthenticatedGuardianRequest,
    ) -> Result<GuardianReply, GuardianRejectionCode> {
        let payload = GuardianSpawnPayload::decode(request.payload())
            .map_err(|_| GuardianRejectionCode::InvalidRequest)?;
        let pane_id = request
            .header()
            .pane_id
            .ok_or(GuardianRejectionCode::InvalidRequest)?;
        let guardian_incarnation = self.incarnation;
        let max_panes = self.config.max_panes;
        let Self {
            protocol,
            registry,
            panes,
            pty_tokens,
            next_pty_token,
            output_pipeline,
            indeterminate_effect,
            ..
        } = self;
        let protocol = protocol
            .as_mut()
            .ok_or(GuardianRejectionCode::InternalInvariant)?;
        effect_result(
            request,
            protocol.apply_effect_transactionally(request, |_| {
                if effective_pane_occupancy(panes.len(), *indeterminate_effect) >= max_panes {
                    return GuardianEffectOutcome::DefinitelyNotApplied(
                        RuntimeEffectError::CapacityExhausted,
                    );
                }
                GuardianEffectOutcome::from_definite_result((|| {
                    let output = output_pipeline
                        .prepare_pane(guardian_incarnation, pane_id)
                        .map_err(|_| RuntimeEffectError::InternalInvariant)?;
                    let input = output_pipeline
                        .prepare_input(guardian_incarnation, pane_id)
                        .map_err(|_| RuntimeEffectError::InternalInvariant)?;
                    spawn_runtime_pane(
                        registry,
                        panes,
                        pty_tokens,
                        next_pty_token,
                        pane_id,
                        payload,
                        output,
                        input,
                    )
                })())
            }),
            indeterminate_effect,
        )
    }

    fn apply_metadata_effect(
        &mut self,
        request: &AuthenticatedGuardianRequest,
    ) -> Result<GuardianReply, GuardianRejectionCode> {
        let Self {
            protocol,
            indeterminate_effect,
            ..
        } = self;
        let protocol = protocol
            .as_mut()
            .ok_or(GuardianRejectionCode::InternalInvariant)?;
        effect_result(
            request,
            protocol.apply_effect_transactionally(request, |_| {
                GuardianEffectOutcome::<RuntimeEffectError>::Applied
            }),
            indeterminate_effect,
        )
    }

    fn apply_resize(
        &mut self,
        request: &AuthenticatedGuardianRequest,
    ) -> Result<GuardianReply, GuardianRejectionCode> {
        let size = GuardianResizePayload::decode(request.payload())
            .map_err(|_| GuardianRejectionCode::InvalidRequest)?
            .size();
        let pane_id = request
            .header()
            .pane_id
            .ok_or(GuardianRejectionCode::InvalidRequest)?;
        let Self {
            protocol,
            panes,
            indeterminate_effect,
            ..
        } = self;
        let protocol = protocol
            .as_mut()
            .ok_or(GuardianRejectionCode::InternalInvariant)?;
        effect_result(
            request,
            protocol.apply_effect_transactionally(request, |_| {
                let Some(pane) = panes.get(&pane_id) else {
                    return GuardianEffectOutcome::DefinitelyNotApplied(
                        RuntimeEffectError::InternalInvariant,
                    );
                };
                classify_external_mutation_result(pane._master.resize(size))
            }),
            indeterminate_effect,
        )
    }

    fn apply_signal(
        &mut self,
        request: &AuthenticatedGuardianRequest,
    ) -> Result<GuardianReply, GuardianRejectionCode> {
        match GuardianSignal::decode(request.payload())
            .map_err(|_| GuardianRejectionCode::InvalidRequest)?
        {
            GuardianSignal::Terminate => {}
        }
        let pane_id = request
            .header()
            .pane_id
            .ok_or(GuardianRejectionCode::InvalidRequest)?;
        let Self {
            protocol,
            panes,
            indeterminate_effect,
            ..
        } = self;
        let protocol = protocol
            .as_mut()
            .ok_or(GuardianRejectionCode::InternalInvariant)?;
        effect_result(
            request,
            protocol.apply_effect_transactionally(request, |_| {
                let Some(pane) = panes.get_mut(&pane_id) else {
                    return GuardianEffectOutcome::DefinitelyNotApplied(
                        RuntimeEffectError::InternalInvariant,
                    );
                };
                classify_external_mutation_result(pane.killer.kill())
            }),
            indeterminate_effect,
        )
    }

    fn apply_close(
        &mut self,
        request: &AuthenticatedGuardianRequest,
    ) -> Result<GuardianReply, GuardianRejectionCode> {
        let pane_id = request
            .header()
            .pane_id
            .ok_or(GuardianRejectionCode::InvalidRequest)?;
        let Self {
            protocol,
            panes,
            indeterminate_effect,
            ..
        } = self;
        let protocol = protocol
            .as_mut()
            .ok_or(GuardianRejectionCode::InternalInvariant)?;
        effect_result(
            request,
            protocol.apply_effect_transactionally(request, |reply| {
                let GuardianReply::MutationApplied { sequence, .. } = reply else {
                    return GuardianEffectOutcome::DefinitelyNotApplied(
                        RuntimeEffectError::InternalInvariant,
                    );
                };
                if *sequence == 0 {
                    return GuardianEffectOutcome::Applied;
                }
                let Some(pane) = panes.get_mut(&pane_id) else {
                    return GuardianEffectOutcome::DefinitelyNotApplied(
                        RuntimeEffectError::InternalInvariant,
                    );
                };
                classify_external_mutation_result(pane.killer.kill())
            }),
            indeterminate_effect,
        )
    }
}

const fn operation_allowed_during_effect_quarantine(operation: GuardianOperation) -> bool {
    matches!(
        operation,
        GuardianOperation::Hello
            | GuardianOperation::Census
            | GuardianOperation::QueryInputEffect
            | GuardianOperation::Attach
    )
}

const fn newly_indeterminate_effect(was_indeterminate: bool, is_indeterminate: bool) -> bool {
    !was_indeterminate && is_indeterminate
}

const fn effective_pane_occupancy(pane_count: usize, indeterminate_effect: bool) -> usize {
    if indeterminate_effect {
        pane_count.saturating_add(1)
    } else {
        pane_count
    }
}

fn pane_is_rearm_candidate(pane: &RuntimePane) -> bool {
    pane.output.waiting_for_slot
        && !pane.output.failed
        && pane.output.pending_plaintext.is_none()
        && pane.output.in_flight_bytes == 0
        && !pane.pty_eof_observed
}

fn round_robin_successor(
    cursor: Option<Uuid>,
    candidates: impl IntoIterator<Item = Uuid>,
) -> Option<Uuid> {
    let mut after_cursor = None;
    let mut wrap = None;
    for candidate in candidates {
        if wrap.is_none_or(|current| candidate < current) {
            wrap = Some(candidate);
        }
        if cursor.is_none_or(|cursor| candidate > cursor)
            && after_cursor.is_none_or(|current| candidate < current)
        {
            after_cursor = Some(candidate);
        }
    }
    after_cursor.or(wrap)
}

const fn terminal_resources_releasable(
    exit_observed: bool,
    pty_eof_observed: bool,
    output_quiescent: bool,
    protocol_terminal: bool,
) -> bool {
    exit_observed && pty_eof_observed && output_quiescent && protocol_terminal
}

fn remaining_output_capacity(
    config: GuardianRuntimeConfig,
    pane_output_bytes: usize,
    total_output_bytes: usize,
) -> usize {
    config
        .max_output_bytes_per_pane
        .saturating_sub(pane_output_bytes)
        .min(
            config
                .max_total_output_bytes
                .saturating_sub(total_output_bytes),
        )
}

#[derive(Clone, Copy, Debug)]
enum RuntimeEffectError {
    CapacityExhausted,
    InternalInvariant,
}

/// An abstract PTY mutation error cannot prove that no effect occurred.
///
/// Neither `ChildKiller::kill` nor `MasterPty::resize` promises that an error
/// proves the underlying OS mutation was not observed. Treating an abstract
/// implementation error as definitely-not-applied would therefore allow the
/// exact mutation to be retried without a causal non-application proof. The
/// only safe generic classification is an indeterminate outcome and permanent
/// protocol quarantine.
fn classify_external_mutation_result<E>(
    result: Result<(), E>,
) -> GuardianEffectOutcome<RuntimeEffectError> {
    match result {
        Ok(()) => GuardianEffectOutcome::Applied,
        Err(_) => GuardianEffectOutcome::OutcomeIndeterminate,
    }
}

fn effect_result(
    request: &AuthenticatedGuardianRequest,
    result: Result<GuardianReply, GuardianEffectTransactionError<RuntimeEffectError>>,
    indeterminate_effect: &mut bool,
) -> Result<GuardianReply, GuardianRejectionCode> {
    match result {
        Ok(reply) => Ok(reply),
        Err(GuardianEffectTransactionError::Protocol(error)) => {
            Err(GuardianRejectionCode::from_protocol_error(&error))
        }
        Err(GuardianEffectTransactionError::Effect(
            RuntimeEffectError::CapacityExhausted,
        )) => Err(GuardianRejectionCode::CapacityExhausted),
        Err(GuardianEffectTransactionError::Effect(
            RuntimeEffectError::InternalInvariant,
        )) => Err(GuardianRejectionCode::InternalInvariant),
        Err(GuardianEffectTransactionError::OutcomeIndeterminate(intended_reply)) => {
            *indeterminate_effect = true;
            GuardianReply::effect_outcome_indeterminate(request, &intended_reply)
                .map_err(|error| GuardianRejectionCode::from_protocol_error(&error))
        }
    }
}

fn spawn_runtime_pane(
    registry: &Registry,
    panes: &mut HashMap<Uuid, RuntimePane>,
    pty_tokens: &mut HashMap<Token, Uuid>,
    next_pty_token: &mut usize,
    pane_id: Uuid,
    payload: GuardianSpawnPayload,
    output: GuardianPaneOutputJournal,
    input_journal: GuardianPaneInputJournal,
) -> Result<(), RuntimeEffectError> {
    if panes.contains_key(&pane_id) || pty_tokens.contains_key(&Token(*next_pty_token)) {
        return Err(RuntimeEffectError::InternalInvariant);
    }
    panes
        .try_reserve(1)
        .map_err(|_| RuntimeEffectError::InternalInvariant)?;
    pty_tokens
        .try_reserve(1)
        .map_err(|_| RuntimeEffectError::InternalInvariant)?;
    let token = Token(*next_pty_token);
    let following_token = next_pty_token
        .checked_add(1)
        .ok_or(RuntimeEffectError::InternalInvariant)?;
    let (command, size) = payload.into_parts();
    let pair = native_pty_system()
        .openpty(size)
        .map_err(|_| RuntimeEffectError::InternalInvariant)?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|_| RuntimeEffectError::InternalInvariant)?;
    let reader = pair
        .master
        .try_clone_pollable_reader()
        .map_err(|_| RuntimeEffectError::InternalInvariant)?;
    let raw_fd = reader.as_fd().as_raw_fd();
    registry
        .register(
            &mut SourceFd(&raw_fd),
            token,
            Interest::READABLE,
        )
        .map_err(|_| RuntimeEffectError::InternalInvariant)?;

    let child = match pair.slave.spawn_command(command) {
        Ok(child) => child,
        Err(_) => {
            let _ = registry.deregister(&mut SourceFd(&raw_fd));
            return Err(RuntimeEffectError::InternalInvariant);
        }
    };
    let killer = child.clone_killer();
    let pane = RuntimePane {
        _master: pair.master,
        writer: Some(writer),
        input_journal: Some(input_journal),
        reader,
        reader_registered: true,
        child,
        killer,
        output: RuntimePaneOutput::new(output),
        pty_eof_observed: false,
        exit_observed: false,
        token,
    };

    pty_tokens.insert(token, pane_id);
    panes.insert(pane_id, pane);
    *next_pty_token = following_token;
    Ok(())
}

fn deregister_reader(
    registry: &Registry,
    pane: &mut RuntimePane,
    counters: &mut GuardianRuntimeCounters,
) {
    if !pane.reader_registered {
        return;
    }
    let raw_fd = pane.reader.as_fd().as_raw_fd();
    if registry.deregister(&mut SourceFd(&raw_fd)).is_ok() {
        pane.reader_registered = false;
    } else {
        // The OS registration state is indeterminate after an error. Never
        // treat the old `true` bit as evidence that a later rearm succeeded:
        // the descriptor may already have been removed from the registry.
        // Retain it only so a readiness event can retry deregistration, and
        // fail the pane closed so no more PTY bytes are read under ambiguity.
        pane.output.failed = true;
        pane.output.waiting_for_slot = false;
        counters.output_deregister_failures = counters
            .output_deregister_failures
            .saturating_add(1);
    }
}

fn register_reader(registry: &Registry, pane: &mut RuntimePane) -> bool {
    if pane.reader_registered {
        return true;
    }
    let raw_fd = pane.reader.as_fd().as_raw_fd();
    if registry
        .register(
            &mut SourceFd(&raw_fd),
            pane.token,
            Interest::READABLE,
        )
        .is_err()
    {
        return false;
    }
    pane.reader_registered = true;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use mio::{Poll, Waker};
    use mux::guardian_protocol::{
        GuardianRequestEnvelope, GuardianRequestHeader, GuardianResponseEnvelope,
        GuardianResponseStatus, GuardianSecret, decode_guardian_request,
        encode_guardian_request,
    };
    use portable_pty::{CommandBuilder, PtySize};
    use std::io;
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    fn runtime_for_input_rejection() -> (std::path::PathBuf, Poll, GuardianRuntime) {
        let canonical_temp =
            std::fs::canonicalize(std::env::temp_dir()).expect("canonical temp directory");
        let directory = tempfile::Builder::new()
            .prefix("ft-guardian-runtime-input-rejection-")
            .tempdir_in(canonical_temp)
            .expect("create private runtime test directory")
            .keep();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
            .expect("secure runtime test directory");
        let poll = Poll::new().expect("create runtime test poll");
        let completion_waker = Arc::new(
            Waker::new(poll.registry(), Token(1)).expect("create runtime completion waker"),
        );
        let output_pipeline = GuardianOutputPipeline::open(
            &directory.join("guardian.token"),
            1,
            Arc::clone(&completion_waker),
        )
        .expect("create runtime output pipeline");
        let config = GuardianRuntimeConfig::new(
            1,
            OUTPUT_RECORD_BYTES,
            OUTPUT_RECORD_BYTES,
            2,
            4,
        )
        .expect("valid runtime test limits");
        let runtime = GuardianRuntime::new(
            poll.registry()
                .try_clone()
                .expect("clone runtime test registry"),
            config,
            Uuid::from_u128(1),
            output_pipeline,
            completion_waker,
        )
        .expect("create guardian runtime");
        (directory, poll, runtime)
    }

    fn authenticated_input_request_for(
        request_id: Uuid,
        pane_id: Uuid,
        effect_id: Uuid,
        payload: &[u8],
    ) -> AuthenticatedGuardianRequest {
        let payload = payload.to_vec();
        let request = GuardianRequestEnvelope::new(
            GuardianRequestHeader::new(
                GuardianOperation::Input,
                Uuid::from_u128(1),
                Uuid::from_u128(2),
                request_id,
                Some(pane_id),
                1,
                1,
                Some(effect_id),
                &payload,
            ),
            payload,
        );
        let secret = GuardianSecret::from_bytes([0x5a; 32]).expect("test secret is strong");
        let frame = encode_guardian_request(&secret, &request).expect("test request encodes");
        decode_guardian_request(&secret, &frame).expect("test request authenticates")
    }

    fn authenticated_input_request() -> AuthenticatedGuardianRequest {
        authenticated_input_request_for(
            Uuid::from_u128(6),
            Uuid::from_u128(3),
            Uuid::from_u128(4),
            b"x",
        )
    }

    fn authenticated_claim_request(
        request_id: Uuid,
        pane_id: Uuid,
        effect_id: Uuid,
    ) -> AuthenticatedGuardianRequest {
        let request = GuardianRequestEnvelope::new(
            GuardianRequestHeader::new(
                GuardianOperation::Claim,
                Uuid::from_u128(1),
                Uuid::from_u128(2),
                request_id,
                Some(pane_id),
                0,
                0,
                Some(effect_id),
                &[],
            ),
            Vec::new(),
        );
        let secret = GuardianSecret::from_bytes([0x5a; 32]).expect("test secret is strong");
        let frame = encode_guardian_request(&secret, &request).expect("test request encodes");
        decode_guardian_request(&secret, &frame).expect("test request authenticates")
    }

    fn authenticated_hello_request(
        request_id: Uuid,
        mux_incarnation: Uuid,
    ) -> AuthenticatedGuardianRequest {
        let request = GuardianRequestEnvelope::new(
            GuardianRequestHeader::new(
                GuardianOperation::Hello,
                Uuid::nil(),
                mux_incarnation,
                request_id,
                None,
                0,
                0,
                None,
                &[],
            ),
            Vec::new(),
        );
        let secret = GuardianSecret::from_bytes([0x5a; 32]).expect("test secret is strong");
        let frame = encode_guardian_request(&secret, &request).expect("test request encodes");
        decode_guardian_request(&secret, &frame).expect("test request authenticates")
    }

    #[derive(Clone, Copy)]
    enum TestWriteMode {
        Full,
        Zero,
        Prefix(usize),
    }

    struct CountingWriter {
        calls: Arc<AtomicUsize>,
        mode: TestWriteMode,
    }

    impl Write for CountingWriter {
        fn write(&mut self, payload: &[u8]) -> io::Result<usize> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(match self.mode {
                TestWriteMode::Full => payload.len(),
                TestWriteMode::Zero => 0,
                TestWriteMode::Prefix(bytes) => bytes.min(payload.len()),
            })
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct BlockingWriter {
        calls: Arc<AtomicUsize>,
        entered: SyncSender<()>,
        release: Receiver<()>,
    }

    impl Write for BlockingWriter {
        fn write(&mut self, payload: &[u8]) -> io::Result<usize> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.entered
                .send(())
                .map_err(|_| io::Error::other("test entry observer disconnected"))?;
            self.release
                .recv()
                .map_err(|_| io::Error::other("test release sender disconnected"))?;
            Ok(payload.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct PanickingWriter {
        calls: Arc<AtomicUsize>,
    }

    impl Write for PanickingWriter {
        fn write(&mut self, _payload: &[u8]) -> io::Result<usize> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            panic!("guardian input writer panic probe")
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn authenticated_spawn_request_for(
        request_id: Uuid,
        pane_id: Uuid,
        effect_id: Uuid,
        command: CommandBuilder,
    ) -> AuthenticatedGuardianRequest {
        let guardian = Uuid::from_u128(1);
        let mux = Uuid::from_u128(2);
        let payload = GuardianSpawnPayload::new(command, PtySize::default())
            .expect("test spawn payload is valid")
            .encode()
            .expect("test spawn payload encodes");
        let request = GuardianRequestEnvelope::new(
            GuardianRequestHeader::new(
                GuardianOperation::Spawn,
                guardian,
                mux,
                request_id,
                Some(pane_id),
                0,
                0,
                Some(effect_id),
                &payload,
            ),
            payload,
        );
        let secret = GuardianSecret::from_bytes([0x5a; 32]).expect("test secret is strong");
        let frame = encode_guardian_request(&secret, &request).expect("test request encodes");
        decode_guardian_request(&secret, &frame).expect("test request authenticates")
    }

    fn authenticated_spawn_request() -> AuthenticatedGuardianRequest {
        authenticated_spawn_request_for(
            Uuid::from_u128(5),
            Uuid::from_u128(3),
            Uuid::from_u128(4),
            CommandBuilder::new("guardian-runtime-indeterminate-test"),
        )
    }

    fn successful_spawn_request(
        request_id: Uuid,
        pane_id: Uuid,
        effect_id: Uuid,
    ) -> AuthenticatedGuardianRequest {
        let mut command = CommandBuilder::new("/bin/sh");
        command.arg("-c");
        command.arg("exit 0");
        authenticated_spawn_request_for(request_id, pane_id, effect_id, command)
    }

    fn claimed_runtime_with_writer(
        writer: Box<dyn Write + Send>,
    ) -> (std::path::PathBuf, Poll, GuardianRuntime, Uuid) {
        let (directory, poll, mut runtime) = runtime_for_input_rejection();
        let pane_id = Uuid::from_u128(31);
        let spawn = successful_spawn_request(
            Uuid::from_u128(32),
            pane_id,
            Uuid::from_u128(33),
        );
        assert_eq!(
            runtime
                .dispatch(&spawn)
                .expect("spawn response")
                .header()
                .status,
            GuardianResponseStatus::Success
        );
        let claim = authenticated_claim_request(
            Uuid::from_u128(34),
            pane_id,
            Uuid::from_u128(35),
        );
        assert_eq!(
            runtime
                .dispatch(&claim)
                .expect("claim response")
                .header()
                .status,
            GuardianResponseStatus::Success
        );
        runtime
            .panes
            .get_mut(&pane_id)
            .expect("spawned pane")
            .writer = Some(writer);
        (directory, poll, runtime, pane_id)
    }

    fn input_route(
        token: usize,
        generation: u64,
        request: &AuthenticatedGuardianRequest,
    ) -> GuardianInputRoute {
        GuardianInputRoute::new(
            Token(token),
            generation,
            request.header().request_id,
            request.header().effect_id.expect("input effect id"),
        )
        .expect("valid input route")
    }

    fn wait_for_input_completion(
        runtime: &mut GuardianRuntime,
    ) -> GuardianRuntimeInputCompletion {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match runtime.try_input_completion() {
                GuardianRuntimeInputCompletionState::Ready(completion) => return completion,
                GuardianRuntimeInputCompletionState::Empty if Instant::now() < deadline => {
                    std::thread::yield_now();
                }
                GuardianRuntimeInputCompletionState::Empty => {
                    panic!("guardian input completion timed out");
                }
                GuardianRuntimeInputCompletionState::Disconnected => {
                    panic!("guardian input worker disconnected");
                }
            }
        }
    }

    fn input_reply_state(response: &GuardianResponseEnvelope) -> InputEffectState {
        let reply = GuardianReply::decode_for_operation(
            GuardianOperation::Input,
            response.payload(),
        )
        .expect("input response payload decodes");
        let GuardianReply::InputReceipt { state, .. } = reply else {
            panic!("input response must carry an input receipt");
        };
        state
    }

    #[test]
    fn borrowed_input_cannot_bypass_owned_worker_submission_or_emit_a_terminal_rejection() {
        let (_directory, _poll, mut runtime) = runtime_for_input_rejection();
        let request = authenticated_input_request();
        let protocol_before = runtime.protocol.clone();
        let pane_count_before = runtime.panes.len();
        let pty_token_count_before = runtime.pty_tokens.len();
        let buffered_output_before = runtime.buffered_output_bytes;
        let indeterminate_before = runtime.indeterminate_effect;
        let counters_before = runtime.counters();

        assert!(runtime.dispatch(&request).is_none());

        let mut expected_counters = counters_before;
        expected_counters.input_activation_rejections = counters_before
            .input_activation_rejections
            .checked_add(1)
            .expect("test rejection counter has headroom");
        assert_eq!(runtime.counters(), expected_counters);
        assert_eq!(runtime.protocol, protocol_before);
        assert_eq!(runtime.panes.len(), pane_count_before);
        assert_eq!(runtime.pty_tokens.len(), pty_token_count_before);
        assert_eq!(runtime.buffered_output_bytes, buffered_output_before);
        assert_eq!(runtime.indeterminate_effect, indeterminate_before);
    }

    #[test]
    fn live_input_commits_before_success_and_exact_retry_never_writes_twice() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (_directory, _poll, mut runtime, pane_id) = claimed_runtime_with_writer(Box::new(
            CountingWriter {
                calls: Arc::clone(&calls),
                mode: TestWriteMode::Full,
            },
        ));
        let request_id = Uuid::from_u128(41);
        let effect_id = Uuid::from_u128(42);
        let request = authenticated_input_request_for(
            request_id,
            pane_id,
            effect_id,
            b"durable-input",
        );
        let route = input_route(7, 1, &request);
        assert!(matches!(
            runtime.submit_input(request, route),
            GuardianInputSubmission::Pending
        ));
        assert!(runtime.protocol.is_none());

        let first = wait_for_input_completion(&mut runtime);
        assert_eq!(first.route, route);
        let first_response = first.response.expect("durable input response");
        assert_eq!(first_response.header().status, GuardianResponseStatus::Success);
        assert_eq!(input_reply_state(&first_response), InputEffectState::DurableFull);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(runtime.protocol.is_some());

        let retry = authenticated_input_request_for(
            request_id,
            pane_id,
            effect_id,
            b"durable-input",
        );
        let retry_route = input_route(8, 2, &retry);
        assert!(matches!(
            runtime.submit_input(retry, retry_route),
            GuardianInputSubmission::Pending
        ));
        let replay = wait_for_input_completion(&mut runtime);
        assert_eq!(replay.route, retry_route);
        assert_eq!(replay.response, Some(first_response));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn zero_byte_write_persists_known_not_applied_and_retries_terminally_reject_without_write() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (_directory, _poll, mut runtime, pane_id) = claimed_runtime_with_writer(Box::new(
            CountingWriter {
                calls: Arc::clone(&calls),
                mode: TestWriteMode::Zero,
            },
        ));
        let request_id = Uuid::from_u128(51);
        let effect_id = Uuid::from_u128(52);
        let request = authenticated_input_request_for(
            request_id,
            pane_id,
            effect_id,
            b"known-zero",
        );
        let route = input_route(7, 1, &request);
        assert!(matches!(
            runtime.submit_input(request, route),
            GuardianInputSubmission::Pending
        ));
        let first = wait_for_input_completion(&mut runtime);
        let first_response = first.response.expect("known-zero terminal response");
        assert_eq!(first_response.header().status, GuardianResponseStatus::Terminal);
        assert_eq!(
            GuardianRejectionCode::decode(
                first_response.header().status,
                first_response.payload(),
            )
            .expect("typed known-zero rejection"),
            GuardianRejectionCode::InputKnownNotApplied
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let retry = authenticated_input_request_for(
            request_id,
            pane_id,
            effect_id,
            b"known-zero",
        );
        let retry_route = input_route(8, 2, &retry);
        assert!(matches!(
            runtime.submit_input(retry, retry_route),
            GuardianInputSubmission::Pending
        ));
        let replay = wait_for_input_completion(&mut runtime);
        assert_eq!(replay.response, Some(first_response));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn partial_write_is_a_durable_terminal_prefix_and_exact_retry_is_inert() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (_directory, _poll, mut runtime, pane_id) = claimed_runtime_with_writer(Box::new(
            CountingWriter {
                calls: Arc::clone(&calls),
                mode: TestWriteMode::Prefix(3),
            },
        ));
        let request_id = Uuid::from_u128(61);
        let effect_id = Uuid::from_u128(62);
        let request = authenticated_input_request_for(
            request_id,
            pane_id,
            effect_id,
            b"partial-input",
        );
        let route = input_route(7, 1, &request);
        assert!(matches!(
            runtime.submit_input(request, route),
            GuardianInputSubmission::Pending
        ));
        let first = wait_for_input_completion(&mut runtime);
        let first_response = first.response.expect("durable prefix response");
        assert_eq!(
            input_reply_state(&first_response),
            InputEffectState::DurablePrefix { applied_bytes: 3 }
        );

        let retry = authenticated_input_request_for(
            request_id,
            pane_id,
            effect_id,
            b"partial-input",
        );
        let retry_route = input_route(8, 2, &retry);
        assert!(matches!(
            runtime.submit_input(retry, retry_route),
            GuardianInputSubmission::Pending
        ));
        let replay = wait_for_input_completion(&mut runtime);
        assert_eq!(replay.response, Some(first_response));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn one_slot_input_authority_closes_a_second_request_retryably_without_a_second_write() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (entered_tx, entered_rx) = sync_channel(1);
        let (release_tx, release_rx) = sync_channel(1);
        let (_directory, _poll, mut runtime, pane_id) = claimed_runtime_with_writer(Box::new(
            BlockingWriter {
                calls: Arc::clone(&calls),
                entered: entered_tx,
                release: release_rx,
            },
        ));
        let first = authenticated_input_request_for(
            Uuid::from_u128(71),
            pane_id,
            Uuid::from_u128(72),
            b"first",
        );
        let first_route = input_route(7, 1, &first);
        assert!(matches!(
            runtime.submit_input(first, first_route),
            GuardianInputSubmission::Pending
        ));
        entered_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("first write entered");

        let hello = authenticated_hello_request(Uuid::from_u128(75), Uuid::from_u128(76));
        let hello_response = runtime
            .dispatch(&hello)
            .expect("cached incarnation serves Hello while protocol is in flight");
        assert_eq!(hello_response.header().status, GuardianResponseStatus::Success);
        assert_eq!(
            GuardianReply::decode_for_operation(
                GuardianOperation::Hello,
                hello_response.payload(),
            )
            .expect("Hello reply decodes"),
            GuardianReply::Hello {
                guardian_incarnation: runtime.incarnation(),
            }
        );

        let second = authenticated_input_request_for(
            Uuid::from_u128(73),
            pane_id,
            Uuid::from_u128(74),
            b"second",
        );
        let second_route = input_route(8, 2, &second);
        assert!(matches!(
            runtime.submit_input(second, second_route),
            GuardianInputSubmission::CloseRetryably
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        release_tx.send(()).expect("release first write");
        let completed = wait_for_input_completion(&mut runtime);
        assert!(completed.response.is_some());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn recovered_writer_panic_retains_every_authority_and_permanently_fences_replay() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (_directory, _poll, mut runtime, pane_id) = claimed_runtime_with_writer(Box::new(
            PanickingWriter {
                calls: Arc::clone(&calls),
            },
        ));
        let request_id = Uuid::from_u128(77);
        let effect_id = Uuid::from_u128(78);
        let request = authenticated_input_request_for(
            request_id,
            pane_id,
            effect_id,
            b"panic-once",
        );
        let route = input_route(7, 1, &request);
        assert!(matches!(
            runtime.submit_input(request, route),
            GuardianInputSubmission::Pending
        ));

        let completion = wait_for_input_completion(&mut runtime);
        assert_eq!(completion.route, route);
        assert!(completion.response.is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(runtime.protocol.is_some());
        let pane = runtime.panes.get(&pane_id).expect("pane authority retained");
        assert!(pane.writer.is_some());
        assert!(pane.input_journal.is_some());
        assert!(runtime.indeterminate_effect);
        assert_eq!(runtime.counters().input_worker_panics, 1);

        let retry = authenticated_input_request_for(
            request_id,
            pane_id,
            effect_id,
            b"panic-once",
        );
        let retry_route = input_route(8, 2, &retry);
        assert!(matches!(
            runtime.submit_input(retry, retry_route),
            GuardianInputSubmission::CloseRetryably
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn final_disconnect_during_input_is_replayed_after_completion_and_retires_the_lease() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (entered_tx, entered_rx) = sync_channel(1);
        let (release_tx, release_rx) = sync_channel(1);
        let (_directory, _poll, mut runtime, pane_id) = claimed_runtime_with_writer(Box::new(
            BlockingWriter {
                calls,
                entered: entered_tx,
                release: release_rx,
            },
        ));
        let request = authenticated_input_request_for(
            Uuid::from_u128(81),
            pane_id,
            Uuid::from_u128(82),
            b"disconnect-race",
        );
        let route = input_route(7, 1, &request);
        assert!(matches!(
            runtime.submit_input(request, route),
            GuardianInputSubmission::Pending
        ));
        entered_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("write entered before disconnect");
        assert_eq!(
            runtime
                .retire_disconnected_mux(Uuid::from_u128(2), 11)
                .expect("defer final disconnect"),
            GuardianMuxLeaseRetirement::default()
        );
        assert_eq!(
            runtime.pending_mux_retirements,
            [PendingMuxRetirement {
                mux_incarnation: Uuid::from_u128(2),
                disconnected_connection_generation: 11,
            }]
        );

        release_tx.send(()).expect("release write");
        let completion = wait_for_input_completion(&mut runtime);
        assert!(completion.response.is_some());
        assert!(runtime.pending_mux_retirements.is_empty());
        assert!(matches!(
            runtime
                .protocol
                .as_ref()
                .and_then(|protocol| protocol.pane_state(pane_id)),
            Some(GuardianPaneState::LiveUnclaimed { generation: 1 })
        ));
    }

    #[test]
    fn authenticated_reconnect_cancels_only_the_older_worker_deferred_retirement() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (entered_tx, entered_rx) = sync_channel(1);
        let (release_tx, release_rx) = sync_channel(1);
        let (_directory, _poll, mut runtime, pane_id) = claimed_runtime_with_writer(Box::new(
            BlockingWriter {
                calls,
                entered: entered_tx,
                release: release_rx,
            },
        ));
        let request = authenticated_input_request_for(
            Uuid::from_u128(91),
            pane_id,
            Uuid::from_u128(92),
            b"disconnect-reconnect-race",
        );
        let route = input_route(7, 11, &request);
        assert!(matches!(
            runtime.submit_input(request, route),
            GuardianInputSubmission::Pending
        ));
        entered_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("write entered before disconnect");

        let mux_incarnation = Uuid::from_u128(2);
        runtime
            .retire_disconnected_mux(mux_incarnation, 11)
            .expect("defer final disconnect while worker owns protocol");
        assert_eq!(runtime.pending_mux_retirements.len(), 1);

        let hello = authenticated_hello_request(Uuid::from_u128(93), mux_incarnation);
        let hello_response = runtime
            .dispatch(&hello)
            .expect("cached Hello succeeds while worker owns protocol");
        assert_eq!(hello_response.header().status, GuardianResponseStatus::Success);

        runtime
            .observe_connected_mux(mux_incarnation, 10)
            .expect("older connection observation is well formed");
        runtime
            .observe_connected_mux(mux_incarnation, 11)
            .expect("equal connection observation is well formed");
        assert_eq!(
            runtime.pending_mux_retirements,
            [PendingMuxRetirement {
                mux_incarnation,
                disconnected_connection_generation: 11,
            }],
            "an older or equal observation cannot cancel the disconnect"
        );
        let unrelated_mux = Uuid::from_u128(94);
        runtime
            .retire_disconnected_mux(unrelated_mux, 9)
            .expect("defer an unrelated mux disconnect");
        runtime
            .observe_connected_mux(mux_incarnation, 12)
            .expect("new authenticated connection generation is accepted");
        assert_eq!(
            runtime.pending_mux_retirements,
            [PendingMuxRetirement {
                mux_incarnation: unrelated_mux,
                disconnected_connection_generation: 9,
            }],
            "the reconnect cancels only its own mux retirement"
        );

        release_tx.send(()).expect("release input write");
        let completion = wait_for_input_completion(&mut runtime);
        assert!(completion.response.is_some());
        assert!(matches!(
            runtime
                .protocol
                .as_ref()
                .and_then(|protocol| protocol.pane_state(pane_id)),
            Some(GuardianPaneState::LiveClaimed {
                generation: 1,
                mux_incarnation: owner,
                ..
            }) if *owner == mux_incarnation
        ));
        assert!(runtime
            .retire_disconnected_mux(mux_incarnation, 0)
            .is_err());
        assert!(runtime.observe_connected_mux(mux_incarnation, 0).is_err());
        assert!(runtime.retire_disconnected_mux(Uuid::nil(), 13).is_err());
        assert!(runtime.observe_connected_mux(Uuid::nil(), 13).is_err());
    }

    #[test]
    fn exact_spawn_retry_at_runtime_capacity_replays_without_a_second_pty() {
        let (_directory, _poll, mut runtime) = runtime_for_input_rejection();
        let pane_id = Uuid::from_u128(31);
        let request =
            successful_spawn_request(Uuid::from_u128(32), pane_id, Uuid::from_u128(33));

        let first = runtime.dispatch(&request).expect("first spawn response");
        assert_eq!(first.header().status, GuardianResponseStatus::Success);
        assert!(runtime.panes.contains_key(&pane_id));
        assert_eq!(runtime.panes.len(), 1);
        assert_eq!(runtime.pty_tokens.len(), 1);
        let next_pty_token_after_first = runtime.next_pty_token;
        let protocol_after_first = runtime.protocol.clone();

        let replay = runtime.dispatch(&request).expect("exact spawn replay response");
        assert_eq!(replay, first);
        assert_eq!(runtime.protocol, protocol_after_first);
        assert_eq!(runtime.panes.len(), 1);
        assert_eq!(runtime.pty_tokens.len(), 1);
        assert_eq!(runtime.next_pty_token, next_pty_token_after_first);

        let distinct = successful_spawn_request(
            Uuid::from_u128(34),
            Uuid::from_u128(35),
            Uuid::from_u128(36),
        );
        let rejected = runtime
            .dispatch(&distinct)
            .expect("capacity rejection response");
        assert_eq!(
            rejected,
            GuardianResponseEnvelope::rejection(
                &distinct,
                GuardianRejectionCode::CapacityExhausted,
            )
        );
        assert_eq!(runtime.protocol, protocol_after_first);
        assert_eq!(runtime.panes.len(), 1);
        assert_eq!(runtime.pty_tokens.len(), 1);
        assert_eq!(runtime.next_pty_token, next_pty_token_after_first);
    }

    #[test]
    fn aggregate_output_budget_is_checked_and_dominates_local_headroom() {
        let config = GuardianRuntimeConfig::new(4, 128, 256, 10, 4).unwrap();
        assert_eq!(remaining_output_capacity(config, 32, 240), 16);
        assert_eq!(remaining_output_capacity(config, 128, 0), 0);
        assert_eq!(remaining_output_capacity(config, 0, 256), 0);

        assert!(GuardianRuntimeConfig::new(2, 128, 257, 10, 2).is_err());
        assert!(GuardianRuntimeConfig::new(2, usize::MAX, 1, 10, 2).is_err());
    }

    #[test]
    fn terminal_resource_release_requires_exit_eof_empty_output_and_terminal_protocol() {
        assert!(terminal_resources_releasable(true, true, true, true));
        assert!(!terminal_resources_releasable(false, true, true, true));
        assert!(!terminal_resources_releasable(true, false, true, true));
        assert!(!terminal_resources_releasable(true, true, false, true));
        assert!(!terminal_resources_releasable(true, true, true, false));
    }

    #[test]
    fn round_robin_rearm_serves_more_waiting_panes_than_fixed_worker_slots(
    ) -> Result<(), &'static str> {
        let pane_ids = (1_u128..=5).map(Uuid::from_u128).collect::<Vec<_>>();
        let worker_slots = 2;
        let mut cursor = OutputRearmCursor::default();
        let mut serviced = Vec::new();

        // Each round models the prior registered panes completing and becoming
        // continuously ready again. A non-rotating HashMap walk would keep
        // selecting the same first two panes and fail this coverage assertion.
        for _ in 0..3 {
            let mut waiting = pane_ids.clone();
            for _ in 0..worker_slots {
                let selected = cursor
                    .select(waiting.iter().copied())
                    .ok_or("a waiting pane should be selected")?;
                serviced.push(selected);
                waiting.retain(|pane_id| *pane_id != selected);
            }
        }
        assert_eq!(
            serviced,
            [
                pane_ids[0],
                pane_ids[1],
                pane_ids[2],
                pane_ids[3],
                pane_ids[4],
                pane_ids[0],
            ]
        );
        assert!(pane_ids.iter().all(|pane_id| serviced.contains(pane_id)));

        let mut stale_cursor = OutputRearmCursor {
            last_serviced: Some(pane_ids[2]),
        };
        assert_eq!(
            stale_cursor.select(
                [pane_ids[0], pane_ids[1], pane_ids[3], pane_ids[4]],
            ),
            Some(pane_ids[3])
        );
        stale_cursor.last_serviced = Some(pane_ids[4]);
        assert_eq!(
            stale_cursor.select([pane_ids[0], pane_ids[1]]),
            Some(pane_ids[0])
        );
        Ok(())
    }

    #[test]
    fn indeterminate_effect_quarantines_every_later_mutation_and_consumes_capacity() {
        let request = authenticated_spawn_request();
        let mut indeterminate = false;
        assert_eq!(
            effect_result(
                &request,
                Err(
                    GuardianEffectTransactionError::<RuntimeEffectError>::OutcomeIndeterminate(
                        GuardianReply::Spawned {
                            pane_id: request.header().pane_id.expect("spawn pane id"),
                            generation: 0,
                        },
                    ),
                ),
                &mut indeterminate,
            )
            .expect("indeterminate effect has a typed receipt"),
            GuardianReply::EffectOutcomeIndeterminate {
                pane_id: request.header().pane_id.expect("spawn pane id"),
                generation: 0,
                sequence: 0,
                effect_id: request.header().effect_id.expect("spawn effect id"),
            }
        );
        assert!(indeterminate);
        assert!(newly_indeterminate_effect(false, indeterminate));
        assert!(!newly_indeterminate_effect(indeterminate, indeterminate));
        assert_eq!(effective_pane_occupancy(3, indeterminate), 4);

        for observation in [
            GuardianOperation::Hello,
            GuardianOperation::Census,
            GuardianOperation::QueryInputEffect,
            GuardianOperation::Attach,
        ] {
            assert!(operation_allowed_during_effect_quarantine(observation));
        }
        for mutation in [
            GuardianOperation::Input,
            GuardianOperation::Spawn,
            GuardianOperation::Claim,
            GuardianOperation::Resize,
            GuardianOperation::Signal,
            GuardianOperation::Close,
            GuardianOperation::Checkpoint,
            GuardianOperation::Replay,
            GuardianOperation::GuardedStop,
            GuardianOperation::RetireLease,
        ] {
            assert!(!operation_allowed_during_effect_quarantine(mutation));
        }
    }

    #[test]
    fn failed_external_mutation_is_never_classified_as_not_applied() {
        assert!(matches!(
            classify_external_mutation_result::<std::io::Error>(Ok(())),
            GuardianEffectOutcome::Applied
        ));
        assert!(matches!(
            classify_external_mutation_result(Err(std::io::Error::other(
                "injected failure after a possible signal delivery"
            ))),
            GuardianEffectOutcome::OutcomeIndeterminate
        ));
    }
}
