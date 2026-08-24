//! Guardian-owned PTY and child lifetime state.

use crate::output::{
    GuardianOutputCompletionState, GuardianOutputPipeline, GuardianOutputSubmitError,
    GuardianPaneOutputJournal, OUTPUT_RECORD_BYTES,
};
use mio::unix::SourceFd;
use mio::{Interest, Registry, Token};
use mux::guardian_protocol::{
    AuthenticatedGuardianRequest, GuardianEffectOutcome, GuardianEffectTransactionError,
    GuardianMuxLeaseRetirement, GuardianOperation, GuardianPaneState, GuardianProtocolError,
    GuardianProtocolState, GuardianRejectionCode, GuardianReply, GuardianResizePayload,
    GuardianResponseEnvelope, GuardianSignal, GuardianSpawnPayload, GUARDIAN_MAX_PANES,
};
use portable_pty::{
    Child, ChildKiller, MasterPty, PollablePtyReader, native_pty_system,
};
use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsFd, AsRawFd};
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
    _writer: Box<dyn Write + Send>,
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

/// One process-local owner of native PTYs, child handles, and fencing state.
pub struct GuardianRuntime {
    protocol: GuardianProtocolState,
    registry: Registry,
    config: GuardianRuntimeConfig,
    panes: HashMap<Uuid, RuntimePane>,
    pty_tokens: HashMap<Token, Uuid>,
    next_pty_token: usize,
    buffered_output_bytes: usize,
    output_pipeline: GuardianOutputPipeline,
    output_pipeline_failed: bool,
    output_rearm_cursor: OutputRearmCursor,
    indeterminate_effect: bool,
    counters: GuardianRuntimeCounters,
}

impl GuardianRuntime {
    pub(crate) fn new(
        registry: Registry,
        config: GuardianRuntimeConfig,
        incarnation: Uuid,
        output_pipeline: GuardianOutputPipeline,
    ) -> Result<Self, GuardianProtocolError> {
        Ok(Self {
            protocol: GuardianProtocolState::new(incarnation)?,
            registry,
            config,
            panes: HashMap::new(),
            pty_tokens: HashMap::new(),
            next_pty_token: config.first_pty_token,
            buffered_output_bytes: 0,
            output_pipeline,
            output_pipeline_failed: false,
            output_rearm_cursor: OutputRearmCursor::default(),
            indeterminate_effect: false,
            counters: GuardianRuntimeCounters::default(),
        })
    }

    #[must_use]
    pub fn incarnation(&self) -> Uuid {
        self.protocol.incarnation()
    }

    #[must_use]
    pub fn pane_count(&self) -> usize {
        effective_pane_occupancy(self.panes.len(), self.indeterminate_effect)
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
    /// Input, checkpoint, and output replay remain fail-closed in this service
    /// slice. Accepting them without the input WAL, checkpoint publisher, and
    /// authenticated output-delivery protocol would create false durability or
    /// replay claims.
    pub fn dispatch(
        &mut self,
        request: &AuthenticatedGuardianRequest,
    ) -> Option<GuardianResponseEnvelope> {
        if self.indeterminate_effect
            && !operation_allowed_during_effect_quarantine(request.header().operation)
        {
            // Only an exact retained indeterminate identity may receive a
            // typed diagnostic receipt. Every new/conflicting mutation closes
            // without a response: a terminal rejection would falsely imply
            // that the earlier external effect definitely did not apply.
            return self
                .protocol
                .indeterminate_effect_reply(request)
                .ok()
                .flatten()
                .and_then(|reply| GuardianResponseEnvelope::reply(request, &reply).ok());
        }
        let effect_was_indeterminate = self.indeterminate_effect;
        let result = match request.header().operation {
            GuardianOperation::Input
            | GuardianOperation::Checkpoint
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

    pub fn retire_disconnected_mux(
        &mut self,
        mux_incarnation: Uuid,
    ) -> Result<GuardianMuxLeaseRetirement, GuardianProtocolError> {
        if self.indeterminate_effect {
            return Err(GuardianProtocolError::StateInvariantViolation(
                "guardian external effects are quarantined after an indeterminate outcome",
            ));
        }
        self.protocol
            .retire_disconnected_mux_leases(mux_incarnation)
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
                    match protocol.mark_exited(*pane_id, exit_status) {
                        Ok(()) | Err(GuardianProtocolError::PaneTerminal) => {
                            pane.exit_observed = true;
                        }
                        Err(_) => {
                            counters.protocol_transition_failures =
                                counters.protocol_transition_failures.saturating_add(1);
                        }
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
            .apply_observation(request)
            .map_err(|error| GuardianRejectionCode::from_protocol_error(&error))
    }

    fn apply_spawn(
        &mut self,
        request: &AuthenticatedGuardianRequest,
    ) -> Result<GuardianReply, GuardianRejectionCode> {
        if effective_pane_occupancy(self.panes.len(), self.indeterminate_effect)
            >= self.config.max_panes
        {
            return Err(GuardianRejectionCode::CapacityExhausted);
        }
        let payload = GuardianSpawnPayload::decode(request.payload())
            .map_err(|_| GuardianRejectionCode::InvalidRequest)?;
        let pane_id = request
            .header()
            .pane_id
            .ok_or(GuardianRejectionCode::InvalidRequest)?;
        let guardian_incarnation = self.protocol.incarnation();
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
        effect_result(
            request,
            protocol.apply_effect_transactionally(request, |_| {
                GuardianEffectOutcome::from_definite_result((|| {
                    let output = output_pipeline
                        .prepare_pane(guardian_incarnation, pane_id)
                        .map_err(|_| RuntimeEffectError)?;
                    spawn_runtime_pane(
                        registry,
                        panes,
                        pty_tokens,
                        next_pty_token,
                        pane_id,
                        payload,
                        output,
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
        effect_result(
            request,
            protocol.apply_effect_transactionally(request, |_| {
                let Some(pane) = panes.get(&pane_id) else {
                    return GuardianEffectOutcome::DefinitelyNotApplied(RuntimeEffectError);
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
        effect_result(
            request,
            protocol.apply_effect_transactionally(request, |_| {
                let Some(pane) = panes.get_mut(&pane_id) else {
                    return GuardianEffectOutcome::DefinitelyNotApplied(RuntimeEffectError);
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
        effect_result(
            request,
            protocol.apply_effect_transactionally(request, |reply| {
                let GuardianReply::MutationApplied { sequence, .. } = reply else {
                    return GuardianEffectOutcome::DefinitelyNotApplied(RuntimeEffectError);
                };
                if *sequence == 0 {
                    return GuardianEffectOutcome::Applied;
                }
                let Some(pane) = panes.get_mut(&pane_id) else {
                    return GuardianEffectOutcome::DefinitelyNotApplied(RuntimeEffectError);
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
struct RuntimeEffectError;

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
        Err(GuardianEffectTransactionError::Effect(_)) => {
            Err(GuardianRejectionCode::InternalInvariant)
        }
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
) -> Result<(), RuntimeEffectError> {
    if panes.contains_key(&pane_id) || pty_tokens.contains_key(&Token(*next_pty_token)) {
        return Err(RuntimeEffectError);
    }
    panes.try_reserve(1).map_err(|_| RuntimeEffectError)?;
    pty_tokens.try_reserve(1).map_err(|_| RuntimeEffectError)?;
    let token = Token(*next_pty_token);
    let following_token = next_pty_token.checked_add(1).ok_or(RuntimeEffectError)?;
    let (command, size) = payload.into_parts();
    let pair = native_pty_system()
        .openpty(size)
        .map_err(|_| RuntimeEffectError)?;
    let writer = pair.master.take_writer().map_err(|_| RuntimeEffectError)?;
    let reader = pair
        .master
        .try_clone_pollable_reader()
        .map_err(|_| RuntimeEffectError)?;
    let raw_fd = reader.as_fd().as_raw_fd();
    registry
        .register(
            &mut SourceFd(&raw_fd),
            token,
            Interest::READABLE,
        )
        .map_err(|_| RuntimeEffectError)?;

    let child = match pair.slave.spawn_command(command) {
        Ok(child) => child,
        Err(_) => {
            let _ = registry.deregister(&mut SourceFd(&raw_fd));
            return Err(RuntimeEffectError);
        }
    };
    let killer = child.clone_killer();
    let pane = RuntimePane {
        _master: pair.master,
        _writer: writer,
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
    use mux::guardian_protocol::{
        GuardianRequestEnvelope, GuardianRequestHeader, GuardianSecret,
        decode_guardian_request, encode_guardian_request,
    };
    use portable_pty::{CommandBuilder, PtySize};

    fn authenticated_spawn_request() -> AuthenticatedGuardianRequest {
        let guardian = Uuid::from_u128(1);
        let mux = Uuid::from_u128(2);
        let pane = Uuid::from_u128(3);
        let effect = Uuid::from_u128(4);
        let payload = GuardianSpawnPayload::new(
            CommandBuilder::new("guardian-runtime-indeterminate-test"),
            PtySize::default(),
        )
        .expect("test spawn payload is valid")
        .encode()
        .expect("test spawn payload encodes");
        let request = GuardianRequestEnvelope::new(
            GuardianRequestHeader::new(
                GuardianOperation::Spawn,
                guardian,
                mux,
                Uuid::from_u128(5),
                Some(pane),
                0,
                0,
                Some(effect),
                &payload,
            ),
            payload,
        );
        let secret = GuardianSecret::from_bytes([0x5a; 32]).expect("test secret is strong");
        let frame = encode_guardian_request(&secret, &request).expect("test request encodes");
        decode_guardian_request(&secret, &frame).expect("test request authenticates")
    }

    #[test]
    fn aggregate_output_budget_is_checked_and_dominates_local_headroom() {
        let config = GuardianRuntimeConfig::new(4, 128, 256, 10).unwrap();
        assert_eq!(remaining_output_capacity(config, 32, 240), 16);
        assert_eq!(remaining_output_capacity(config, 128, 0), 0);
        assert_eq!(remaining_output_capacity(config, 0, 256), 0);

        assert!(GuardianRuntimeConfig::new(2, 128, 257, 10).is_err());
        assert!(GuardianRuntimeConfig::new(2, usize::MAX, 1, 10).is_err());
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
