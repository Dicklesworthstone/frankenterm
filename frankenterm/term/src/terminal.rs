use super::*;
use crate::terminalstate::performer::Performer;
use frankenterm_escape_parser::parser::{Parser, RecoveryGroundBoundary};
#[cfg(feature = "use_serde")]
use std::collections::HashMap;
use std::sync::Arc;
#[cfg(feature = "use_serde")]
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

/// Versioned identity for terminal-model semantics used by guardian suffix
/// replay. Bump this whenever Performer, width, eviction, reset, or checkpoint
/// semantics can map the same parsed actions to different terminal state.
pub const RECOVERY_TERMINAL_REPLAY_SEMANTICS_ID: &str =
    "frankenterm.term.recovery-replay-semantics.v2";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
pub enum ClipboardSelection {
    Clipboard,
    PrimarySelection,
}

pub trait Clipboard: Send + Sync {
    fn set_contents(
        &self,
        selection: ClipboardSelection,
        data: Option<String>,
    ) -> anyhow::Result<()>;
}

impl Clipboard for Box<dyn Clipboard> {
    fn set_contents(
        &self,
        selection: ClipboardSelection,
        data: Option<String>,
    ) -> anyhow::Result<()> {
        self.as_ref().set_contents(selection, data)
    }
}

pub trait DeviceControlHandler: Send + Sync {
    fn handle_device_control(&mut self, _control: frankenterm_escape_parser::DeviceControlMode);
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
pub enum Progress {
    #[default]
    None,
    Percentage(u8),
    Error(u8),
    Indeterminate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
pub enum Alert {
    Bell,
    ToastNotification {
        /// The title text for the notification.
        title: Option<String>,
        /// The message body
        body: String,
        /// Whether clicking on the notification should focus the
        /// window/tab/pane that generated it
        focus: bool,
    },
    CurrentWorkingDirectoryChanged,
    IconTitleChanged(Option<String>),
    WindowTitleChanged(String),
    TabTitleChanged(Option<String>),
    /// When the color palette has been updated
    PaletteChanged,
    /// A UserVar has changed value
    SetUserVar {
        name: String,
        value: String,
    },
    /// When something bumps the seqno in the terminal model and
    /// the terminal is not focused
    OutputSinceFocusLost,
    /// A change to the progress bar state
    Progress(Progress),
    /// An app sent `OSC 1337;SetProfile=<name>` to request a profile
    /// switch. Per ft-fy4ty's security gate, the term layer never
    /// applies the switch silently — it surfaces the request to the
    /// embedder via this alert and lets the embedder show a user
    /// confirmation prompt before any profile mutation happens.
    /// Embedders that want the legacy iTerm2 silent-switch behavior
    /// must opt in via their own confirmation logic.
    SetProfileRequested {
        /// The profile name the application asked for.
        name: String,
    },
    /// An app sent `OSC 22;<shape>` to request a mouse cursor shape
    /// change. The argument is the free-form W3C-style cursor name
    /// (`pointer`, `text`, `wait`, …). Per ft-7yiu2 the term layer
    /// just routes the request through; the GUI maps the string to
    /// a native cursor. Unrecognized values are dropped by the
    /// embedder, not by the term layer.
    MouseShapeRequested {
        /// The shape name, as supplied by the application.
        shape: String,
    },
    /// A Kitty graphics image carried alt-text suitable for
    /// accessibility announcement after sanitization.
    ImageAltText {
        /// The admitted Kitty image id.
        image_id: u32,
        /// Sanitized screen-reader text.
        text: String,
    },
}

pub trait AlertHandler: Send + Sync {
    fn alert(&mut self, alert: Alert);
}

pub trait DownloadHandler: Send + Sync {
    fn save_to_downloads(&self, name: Option<String>, data: Vec<u8>);
}

/// Represents an instance of a terminal emulator.
pub struct Terminal {
    /// The terminal model/state
    state: TerminalState,
    /// Baseline terminal escape sequence parser
    parser: Parser,
}

/// Opaque canonical checkpoint captured from one terminal while its own parser
/// was recovery-ground.  The fields are deliberately private so a guardian
/// cannot pair state bytes with an unrelated parser instance.
#[cfg(feature = "use_serde")]
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct RecoveryTerminalCheckpointV2 {
    canonical_payload: Zeroizing<Vec<u8>>,
    rows: usize,
    cols: usize,
    parser_stream_bytes: u64,
}

#[cfg(feature = "use_serde")]
impl RecoveryTerminalCheckpointV2 {
    #[must_use]
    pub fn canonical_payload(&self) -> &[u8] {
        &self.canonical_payload
    }

    #[must_use]
    pub const fn rows(&self) -> usize {
        self.rows
    }

    #[must_use]
    pub const fn cols(&self) -> usize {
        self.cols
    }

    /// Exact cumulative raw-byte watermark of the parser-ground witness used
    /// for this model capture. Guardian code must bind this number to the
    /// authenticated output-journal receipt; it is not inferred from payload
    /// size or record sequence.
    #[must_use]
    pub const fn parser_stream_bytes(&self) -> u64 {
        self.parser_stream_bytes
    }

    /// Consume the checkpoint while keeping its plaintext payload under an
    /// automatic wipe-on-drop guard.
    #[must_use]
    pub fn into_canonical_payload(mut self) -> Zeroizing<Vec<u8>> {
        std::mem::take(&mut self.canonical_payload)
    }
}

#[cfg(feature = "use_serde")]
impl std::fmt::Debug for RecoveryTerminalCheckpointV2 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RecoveryTerminalCheckpointV2")
            .field("canonical_payload", &"[REDACTED]")
            .field("payload_bytes", &self.canonical_payload.len())
            .field("rows", &self.rows)
            .field("cols", &self.cols)
            .field("parser_stream_bytes", &self.parser_stream_bytes)
            .finish()
    }
}

#[cfg(feature = "use_serde")]
#[derive(Debug, Eq, PartialEq)]
pub enum RecoveryTerminalCheckpointError {
    ParserNotRecoveryGround,
    Checkpoint(crate::terminalstate::checkpoint::TerminalCheckpointError),
}

#[cfg(feature = "use_serde")]
impl std::fmt::Display for RecoveryTerminalCheckpointError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ParserNotRecoveryGround => {
                formatter.write_str("terminal parser is not at a recoverable output boundary")
            }
            Self::Checkpoint(error) => std::fmt::Display::fmt(error, formatter),
        }
    }
}

#[cfg(feature = "use_serde")]
impl std::error::Error for RecoveryTerminalCheckpointError {}

/// Off-topology terminal used for authenticated guardian replay.  It exposes
/// only capability-gated action application, semantic checkpointing, and a
/// consuming transition to a live writer.
#[cfg(feature = "use_serde")]
pub struct InertTerminal {
    terminal: Terminal,
    replay_projection: crate::terminalstate::checkpoint::CheckpointReplayConfigV2,
    custom_cell_width_maps: Vec<Arc<HashMap<u32, u8>>>,
    intended_live_config: Arc<dyn TerminalConfiguration>,
    intended_live_config_revision: crate::config::TerminalConfigurationRevision,
    checkpoint_limits: crate::terminalstate::checkpoint::TerminalCheckpointLimits,
    replayed_records: usize,
    replayed_bytes: usize,
    replay_failed: bool,
    activation_poisoned: bool,
    #[cfg(test)]
    force_writer_preparation_failure: bool,
}

#[cfg(feature = "use_serde")]
#[derive(Debug, Eq, PartialEq)]
pub enum InertTerminalError {
    EmptyReplayRecord,
    ReplayResourceLimit {
        resource: &'static str,
        observed: usize,
        maximum: usize,
    },
    ReplayAccountingOverflow(&'static str),
    ReplayActionAllocation,
    ReplayStringSequence(frankenterm_escape_parser::StringSequenceError),
    ReplayPoisoned,
    ActivationPoisoned,
    ParserNotRecoveryGround,
    UnsupportedGraphicsAction,
    ReplayConfigurationMismatch,
    LiveConfigurationChanged,
    ScrollbackActivation(crate::config::ScrollbackActivationError),
    WriterActivation,
    Checkpoint(crate::terminalstate::checkpoint::TerminalCheckpointError),
}

#[cfg(feature = "use_serde")]
impl std::fmt::Display for InertTerminalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyReplayRecord => {
                formatter.write_str("guardian replay record must not be empty")
            }
            Self::ReplayResourceLimit {
                resource,
                observed,
                maximum,
            } => write!(
                formatter,
                "guardian replay {resource} exceeds its limit: {observed} > {maximum}"
            ),
            Self::ReplayAccountingOverflow(resource) => {
                write!(
                    formatter,
                    "guardian replay {resource} accounting overflowed"
                )
            }
            Self::ReplayActionAllocation => {
                formatter.write_str("guardian replay could not reserve its action batch")
            }
            Self::ReplayStringSequence(_) => formatter.write_str(
                "guardian replay parser rejected an oversized or unallocatable string sequence",
            ),
            Self::ReplayPoisoned => {
                formatter.write_str("guardian replay was permanently poisoned by an earlier error")
            }
            Self::ActivationPoisoned => formatter.write_str(
                "guardian activation is quarantined after an indeterminate cold-store publication",
            ),
            Self::ParserNotRecoveryGround => formatter
                .write_str("guardian replay parser is not at a recoverable output boundary"),
            Self::UnsupportedGraphicsAction => formatter.write_str(
                "guardian replay rejected a graphics action with external or uncheckpointed state",
            ),
            Self::ReplayConfigurationMismatch => formatter.write_str(
                "guardian replay configuration does not match the intended live configuration",
            ),
            Self::LiveConfigurationChanged => {
                formatter.write_str("intended live configuration changed during guardian recovery")
            }
            Self::ScrollbackActivation(error) => std::fmt::Display::fmt(error, formatter),
            Self::WriterActivation => {
                formatter.write_str("guardian replay could not activate the live writer")
            }
            Self::Checkpoint(error) => std::fmt::Display::fmt(error, formatter),
        }
    }
}

#[cfg(feature = "use_serde")]
impl std::error::Error for InertTerminalError {}

/// A failed consuming activation that returns ownership of the intact inert
/// model so callers can retry an ordinary pre-publication failure. An
/// indeterminate publication permanently poisons the returned model: callers
/// must reopen and reconcile the sink's authenticated manifest rather than
/// blindly retrying the checkpoint generation. The contained error is
/// content-free; `Debug` never prints terminal rows.
#[cfg(feature = "use_serde")]
pub struct InertTerminalActivationFailure {
    error: InertTerminalError,
    inert_terminal: InertTerminal,
}

#[cfg(feature = "use_serde")]
impl InertTerminalActivationFailure {
    pub const fn error(&self) -> &InertTerminalError {
        &self.error
    }

    pub fn into_parts(self) -> (InertTerminalError, InertTerminal) {
        (self.error, self.inert_terminal)
    }
}

#[cfg(feature = "use_serde")]
impl std::fmt::Debug for InertTerminalActivationFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InertTerminalActivationFailure")
            .field("error", &self.error)
            .field("inert_terminal", &self.inert_terminal)
            .finish()
    }
}

#[cfg(feature = "use_serde")]
impl std::fmt::Display for InertTerminalActivationFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, formatter)
    }
}

#[cfg(feature = "use_serde")]
impl std::error::Error for InertTerminalActivationFailure {}

#[cfg(feature = "use_serde")]
impl std::fmt::Debug for InertTerminal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InertTerminal")
            .field("seqno", &self.terminal.current_seqno())
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "use_serde")]
impl InertTerminal {
    pub(crate) fn from_restored_state(
        state: TerminalState,
        replay_projection: crate::terminalstate::checkpoint::CheckpointReplayConfigV2,
        custom_cell_width_maps: Vec<Arc<HashMap<u32, u8>>>,
        intended_live_config: Arc<dyn TerminalConfiguration>,
        intended_live_config_revision: crate::config::TerminalConfigurationRevision,
        checkpoint_limits: crate::terminalstate::checkpoint::TerminalCheckpointLimits,
    ) -> Self {
        let parser_string_limit = checkpoint_limits
            .max_string_bytes
            .min(checkpoint_limits.max_replay_total_bytes);
        Self {
            terminal: Terminal::from_restored_state(state, parser_string_limit),
            replay_projection,
            custom_cell_width_maps,
            intended_live_config,
            intended_live_config_revision,
            checkpoint_limits,
            replayed_records: 0,
            replayed_bytes: 0,
            replay_failed: false,
            activation_poisoned: false,
            #[cfg(test)]
            force_writer_preparation_failure: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn force_writer_preparation_failure_for_test(&mut self) {
        self.force_writer_preparation_failure = true;
    }

    #[cfg(test)]
    pub(crate) fn writer_is_inert_for_test(&self) -> bool {
        self.terminal.state.writer_is_inert_for_test()
    }

    /// Replay one authenticated raw-output journal record through this
    /// terminal's owned parser. The full record is parsed into a fallibly grown
    /// batch and capability-checked before Performer sees its first action.
    /// Persistent model usage is re-audited after the batch; a violating batch
    /// may transiently exceed the semantic envelope by at most one bounded
    /// record, but permanently poisons the off-topology terminal before it can
    /// be checkpointed or activated. Any error poisons replay because its
    /// parser may already have consumed bytes that cannot be skipped or retried.
    pub fn replay_bytes(&mut self, bytes: &[u8]) -> Result<(), InertTerminalError> {
        use frankenterm_escape_parser::osc::ITermProprietary;
        use frankenterm_escape_parser::{Action, OperatingSystemCommand};

        if self.activation_poisoned {
            return Err(InertTerminalError::ActivationPoisoned);
        }
        if self.replay_failed {
            return Err(InertTerminalError::ReplayPoisoned);
        }
        if bytes.is_empty() {
            self.replay_failed = true;
            return Err(InertTerminalError::EmptyReplayRecord);
        }
        if bytes.len() > self.checkpoint_limits.max_replay_record_bytes {
            self.replay_failed = true;
            return Err(InertTerminalError::ReplayResourceLimit {
                resource: "record_bytes",
                observed: bytes.len(),
                maximum: self.checkpoint_limits.max_replay_record_bytes,
            });
        }
        let next_total_bytes = self
            .replayed_bytes
            .checked_add(bytes.len())
            .ok_or_else(|| {
                self.replay_failed = true;
                InertTerminalError::ReplayAccountingOverflow("total_bytes")
            })?;
        if next_total_bytes > self.checkpoint_limits.max_replay_total_bytes {
            self.replay_failed = true;
            return Err(InertTerminalError::ReplayResourceLimit {
                resource: "total_bytes",
                observed: next_total_bytes,
                maximum: self.checkpoint_limits.max_replay_total_bytes,
            });
        }
        let next_record_count = self.replayed_records.checked_add(1).ok_or_else(|| {
            self.replay_failed = true;
            InertTerminalError::ReplayAccountingOverflow("record_count")
        })?;
        if next_record_count > self.checkpoint_limits.max_replay_records {
            self.replay_failed = true;
            return Err(InertTerminalError::ReplayResourceLimit {
                resource: "record_count",
                observed: next_record_count,
                maximum: self.checkpoint_limits.max_replay_records,
            });
        }
        if self
            .terminal
            .current_seqno()
            .checked_add(1)
            .is_none_or(|next| next == SequenceNo::MAX)
        {
            self.replay_failed = true;
            return Err(InertTerminalError::ReplayAccountingOverflow(
                "terminal_seqno",
            ));
        }

        let mut actions = Vec::new();
        let mut allocation_failed = false;
        let mut action_limit_exceeded = false;
        let mut action_memory_limit_exceeded = None;
        let max_actions = self.checkpoint_limits.max_replay_actions_per_record;
        let max_action_batch_bytes = self.checkpoint_limits.max_retained_capture_bytes;
        const ACTION_RESERVE_CHUNK: usize = 256;
        self.terminal.parser.parse(bytes, |action| {
            if allocation_failed || action_limit_exceeded || action_memory_limit_exceeded.is_some()
            {
                return;
            }
            if actions.len() >= max_actions {
                action_limit_exceeded = true;
                return;
            }
            if actions.len() == actions.capacity() {
                let additional = max_actions
                    .saturating_sub(actions.len())
                    .min(ACTION_RESERVE_CHUNK);
                if actions.try_reserve_exact(additional).is_err() {
                    allocation_failed = true;
                    return;
                }
                let retained_bytes = actions
                    .capacity()
                    .saturating_mul(std::mem::size_of::<Action>());
                if retained_bytes > max_action_batch_bytes {
                    action_memory_limit_exceeded = Some(retained_bytes);
                    return;
                }
            }
            actions.push(action);
        });
        if let Some(error) = self.terminal.parser.take_string_sequence_error() {
            self.replay_failed = true;
            return Err(InertTerminalError::ReplayStringSequence(error));
        }
        if allocation_failed {
            self.replay_failed = true;
            return Err(InertTerminalError::ReplayActionAllocation);
        }
        if let Some(observed) = action_memory_limit_exceeded {
            self.replay_failed = true;
            return Err(InertTerminalError::ReplayResourceLimit {
                resource: "action_batch_bytes",
                observed,
                maximum: max_action_batch_bytes,
            });
        }
        if action_limit_exceeded {
            self.replay_failed = true;
            return Err(InertTerminalError::ReplayResourceLimit {
                resource: "actions_per_record",
                observed: self
                    .checkpoint_limits
                    .max_replay_actions_per_record
                    .saturating_add(1),
                maximum: self.checkpoint_limits.max_replay_actions_per_record,
            });
        }

        let unsupported = actions.iter().any(|action| match action {
            Action::Sixel(_) | Action::KittyImage(_) => true,
            Action::OperatingSystemCommand(command) => matches!(
                command.as_ref(),
                OperatingSystemCommand::ITermProprietary(ITermProprietary::File(file))
                    if file.inline
            ),
            _ => false,
        });
        if unsupported {
            self.replay_failed = true;
            return Err(InertTerminalError::UnsupportedGraphicsAction);
        }
        self.terminal.perform_actions(actions);
        if let Err(error) =
            crate::terminalstate::checkpoint::TerminalCheckpointV2::validate_inert_replay_resources(
                &self.terminal,
                &self.replay_projection,
                &self.custom_cell_width_maps,
                self.checkpoint_limits,
            )
        {
            self.replay_failed = true;
            return Err(InertTerminalError::Checkpoint(error));
        }
        self.replayed_bytes = next_total_bytes;
        self.replayed_records = next_record_count;
        Ok(())
    }

    pub fn checkpoint(
        &self,
    ) -> Result<crate::terminalstate::checkpoint::TerminalCheckpointV2, InertTerminalError> {
        if self.activation_poisoned {
            return Err(InertTerminalError::ActivationPoisoned);
        }
        if self.replay_failed {
            return Err(InertTerminalError::ReplayPoisoned);
        }
        if !self.terminal.parser.is_recovery_ground() {
            return Err(InertTerminalError::ParserNotRecoveryGround);
        }
        crate::terminalstate::checkpoint::TerminalCheckpointV2::capture_with_limits(
            &self.terminal,
            self.checkpoint_limits,
        )
        .map_err(InertTerminalError::Checkpoint)
    }

    /// Consume the off-topology model and replace its entire discard writer
    /// with a newly spawned live writer.  Buffered replay replies are owned by
    /// the discarded writer object and cannot cross this transition.
    pub fn into_live(
        mut self,
        writer: Box<dyn std::io::Write + Send>,
    ) -> Result<Terminal, InertTerminalActivationFailure> {
        if self.activation_poisoned {
            return Err(InertTerminalActivationFailure {
                error: InertTerminalError::ActivationPoisoned,
                inert_terminal: self,
            });
        }
        if self.replay_failed {
            return Err(InertTerminalActivationFailure {
                error: InertTerminalError::ReplayPoisoned,
                inert_terminal: self,
            });
        }
        if !self.terminal.parser.is_recovery_ground() {
            return Err(InertTerminalActivationFailure {
                error: InertTerminalError::ParserNotRecoveryGround,
                inert_terminal: self,
            });
        }
        #[cfg(test)]
        if self.force_writer_preparation_failure {
            return Err(InertTerminalActivationFailure {
                error: InertTerminalError::WriterActivation,
                inert_terminal: self,
            });
        }
        let prepared_writer = match self.terminal.state.prepare_inert_writer(writer) {
            Ok(prepared) => prepared,
            Err(_) => {
                return Err(InertTerminalActivationFailure {
                    error: InertTerminalError::WriterActivation,
                    inert_terminal: self,
                });
            }
        };
        let live_config = Arc::clone(&self.intended_live_config);
        let activation = {
            let _lease = live_config.acquire_recovery_activation_lease();
            (|| -> Result<(), InertTerminalError> {
                if live_config.revision() != self.intended_live_config_revision {
                    return Err(InertTerminalError::LiveConfigurationChanged);
                }
                let live_projection_matches = self
                    .replay_projection
                    .matches_stable(
                        live_config.as_ref(),
                        self.checkpoint_limits,
                        &self.custom_cell_width_maps,
                    )
                    .map_err(InertTerminalError::Checkpoint)?;
                if !live_projection_matches {
                    return Err(InertTerminalError::ReplayConfigurationMismatch);
                }
                let prepared_config = self
                    .terminal
                    .state
                    .prepare_recovery_configuration(Arc::clone(&live_config));
                self.terminal
                    .state
                    .finish_recovery_activation(prepared_config, prepared_writer)
                    .map_err(InertTerminalError::ScrollbackActivation)?;
                Ok(())
            })()
        };
        if let Err(error) = activation {
            if matches!(
                &error,
                InertTerminalError::ScrollbackActivation(activation_error)
                    if activation_error.outcome_is_indeterminate()
            ) {
                self.activation_poisoned = true;
            }
            return Err(InertTerminalActivationFailure {
                error,
                inert_terminal: self,
            });
        }
        Ok(self.terminal)
    }
}

impl Deref for Terminal {
    type Target = TerminalState;

    fn deref(&self) -> &TerminalState {
        &self.state
    }
}

impl DerefMut for Terminal {
    fn deref_mut(&mut self) -> &mut TerminalState {
        &mut self.state
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, FromDynamic, ToDynamic)]
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
pub struct TerminalSize {
    pub rows: usize,
    pub cols: usize,
    pub pixel_width: usize,
    pub pixel_height: usize,
    pub dpi: u32,
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
            dpi: 0,
        }
    }
}

impl Terminal {
    pub(crate) fn from_restored_state(
        state: TerminalState,
        max_string_sequence_bytes: usize,
    ) -> Self {
        Self {
            state,
            parser: Parser::new_with_max_string_sequence_bytes(max_string_sequence_bytes),
        }
    }

    /// Capture one bounded canonical recovery payload from this terminal's
    /// model only when this terminal's own parser can be replaced by a fresh
    /// parser at the same boundary.
    #[cfg(feature = "use_serde")]
    pub fn capture_recovery_checkpoint(
        &self,
        limits: crate::terminalstate::checkpoint::TerminalCheckpointLimits,
    ) -> Result<RecoveryTerminalCheckpointV2, RecoveryTerminalCheckpointError> {
        let ground = self
            .parser
            .recovery_ground_boundary()
            .ok_or(RecoveryTerminalCheckpointError::ParserNotRecoveryGround)?;
        self.capture_recovery_checkpoint_at_stream_watermark(ground.stream_bytes(), limits)
    }

    /// Capture the model at a typed ground boundary from the external parser
    /// that actually feeds this terminal.
    ///
    /// The witness is non-constructible and immutably borrows its parser, so
    /// that parser cannot consume more bytes until this capture returns. The
    /// embedding mux must additionally hold the terminal/model lock and bind
    /// [`RecoveryTerminalCheckpointV2::parser_stream_bytes`] to its durable
    /// output-journal receipt; this method does not claim that higher-level
    /// delivery ordering on its own.
    #[cfg(feature = "use_serde")]
    pub fn capture_recovery_checkpoint_at_external_parser_ground(
        &self,
        ground: RecoveryGroundBoundary<'_>,
        limits: crate::terminalstate::checkpoint::TerminalCheckpointLimits,
    ) -> Result<RecoveryTerminalCheckpointV2, RecoveryTerminalCheckpointError> {
        self.capture_recovery_checkpoint_at_stream_watermark(ground.stream_bytes(), limits)
    }

    #[cfg(feature = "use_serde")]
    fn capture_recovery_checkpoint_at_stream_watermark(
        &self,
        parser_stream_bytes: u64,
        limits: crate::terminalstate::checkpoint::TerminalCheckpointLimits,
    ) -> Result<RecoveryTerminalCheckpointV2, RecoveryTerminalCheckpointError> {
        let checkpoint =
            crate::terminalstate::checkpoint::TerminalCheckpointV2::capture_with_limits(
                &self.state,
                limits,
            )
            .map_err(RecoveryTerminalCheckpointError::Checkpoint)?;
        let canonical_payload = checkpoint
            .to_canonical_json(limits)
            .map_err(RecoveryTerminalCheckpointError::Checkpoint)?;
        let size = self.state.get_size();
        Ok(RecoveryTerminalCheckpointV2 {
            canonical_payload,
            rows: size.rows,
            cols: size.cols,
            parser_stream_bytes,
        })
    }

    /// Construct a new Terminal.
    /// `physical_rows` and `physical_cols` describe the dimensions
    /// of the visible portion of the terminal display in terms of
    /// the number of text cells.
    ///
    /// `pixel_width` and `pixel_height` describe the dimensions of
    /// that same visible area but in pixels.
    ///
    /// `term_program` and `term_version` are required to identify
    /// the host terminal program; they are used to respond to the
    /// terminal identification sequence `\033[>q`.
    ///
    /// `writer` is anything that implements `std::io::Write`; it
    /// is used to send input to the connected program; both keyboard
    /// and mouse input is encoded and written to that stream, as
    /// are answerback responses to a number of escape sequences.
    pub fn new(
        size: TerminalSize,
        config: Arc<dyn TerminalConfiguration + Send + Sync>,
        term_program: &str,
        term_version: &str,
        // writing to the writer sends data to input of the pty
        writer: Box<dyn std::io::Write + Send>,
    ) -> Terminal {
        Terminal {
            state: TerminalState::new(size, config, term_program, term_version, writer),
            parser: Parser::new(),
        }
    }

    /// Feed the terminal parser a slice of bytes from the output
    /// of the associated program.
    /// The slice is not required to be a complete sequence of escape
    /// characters; it is valid to feed in chunks of data as they arrive.
    /// The output is parsed and applied to the terminal model.
    pub fn advance_bytes<B: AsRef<[u8]>>(&mut self, bytes: B) {
        self.state.increment_seqno();
        {
            let bytes = bytes.as_ref();

            let mut performer = Performer::new(&mut self.state);

            self.parser.parse(bytes, |action| performer.perform(action));
        }
        if let Some(error) = self.parser.take_string_sequence_error() {
            log::warn!(
                "terminal parser discarded an oversized or unallocatable string sequence: {error}"
            );
        }
        self.trigger_unseen_output_notif();
    }

    pub fn perform_actions(&mut self, actions: Vec<frankenterm_escape_parser::Action>) {
        self.state.increment_seqno();
        {
            let mut performer = Performer::new(&mut self.state);
            for action in actions {
                performer.perform(action);
            }
        }
        self.trigger_unseen_output_notif();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::ColorPalette;
    use crate::{CellAttributes, CursorPosition, Line};
    use proptest::prelude::*;
    use std::sync::Arc;

    #[derive(Debug)]
    struct PropTermConfig;

    impl TerminalConfiguration for PropTermConfig {
        fn scrollback_size(&self) -> usize {
            64
        }

        fn color_palette(&self) -> ColorPalette {
            ColorPalette::default()
        }
    }

    #[derive(Debug)]
    struct ScrollbackPropTermConfig {
        scrollback: usize,
    }

    impl TerminalConfiguration for ScrollbackPropTermConfig {
        fn scrollback_size(&self) -> usize {
            self.scrollback
        }

        fn color_palette(&self) -> ColorPalette {
            ColorPalette::default()
        }
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn recovery_checkpoint_requires_its_own_parser_to_be_ground() {
        for incomplete in [
            b"\x1b[".as_slice(),
            b"\x1b]2;unfinished".as_slice(),
            b"\x1bPq".as_slice(),
            b"\xc3".as_slice(),
        ] {
            let mut terminal = Terminal::new(
                TerminalSize {
                    rows: 24,
                    cols: 80,
                    pixel_width: 640,
                    pixel_height: 384,
                    dpi: 96,
                },
                Arc::new(PropTermConfig),
                "FrankenTerm",
                "recovery-checkpoint-test",
                Box::new(Vec::<u8>::new()),
            );
            terminal.advance_bytes(incomplete);

            assert!(matches!(
                terminal.capture_recovery_checkpoint(
                    crate::terminalstate::checkpoint::TerminalCheckpointLimits::default(),
                ),
                Err(RecoveryTerminalCheckpointError::ParserNotRecoveryGround)
            ));
        }
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn inert_replay_bounds_actions_flushed_by_a_split_csi_record() {
        let limits = crate::terminalstate::checkpoint::TerminalCheckpointLimits {
            max_replay_actions_per_record: 4,
            ..crate::terminalstate::checkpoint::TerminalCheckpointLimits::default()
        };
        let config: Arc<dyn TerminalConfiguration + Send + Sync> = Arc::new(PropTermConfig);
        let terminal = Terminal::new(
            TerminalSize::default(),
            Arc::clone(&config),
            "FrankenTerm",
            "split-csi-replay-test",
            Box::new(Vec::<u8>::new()),
        );
        let checkpoint = terminal
            .capture_recovery_checkpoint(limits)
            .expect("capture recovery checkpoint");
        let mut inert =
            crate::terminalstate::checkpoint::TerminalCheckpointV2::decode_canonical_json(
                checkpoint.canonical_payload(),
                limits,
            )
            .expect("validate recovery checkpoint")
            .restore_inert(config)
            .expect("restore inert terminal");

        inert
            .replay_bytes(b"\x1b[1;2;3;4;5")
            .expect("retain incomplete CSI in parser");
        assert!(matches!(
            inert.replay_bytes(b"m"),
            Err(InertTerminalError::ReplayResourceLimit {
                resource: "actions_per_record",
                observed: 5,
                maximum: 4,
            })
        ));
    }

    #[derive(Debug, Clone, PartialEq)]
    struct LineSnapshot {
        text: String,
        wrapped: bool,
        cells: Vec<(String, usize, CellAttributes)>,
    }

    #[derive(Debug, Clone, PartialEq)]
    struct TerminalSnapshot {
        cursor: CursorPosition,
        title: String,
        current_dir: Option<String>,
        progress: Progress,
        palette: ColorPalette,
        all_lines: Vec<LineSnapshot>,
    }

    fn make_prop_term(rows: usize, cols: usize) -> Terminal {
        Terminal::new(
            TerminalSize {
                rows,
                cols,
                pixel_width: cols * 8,
                pixel_height: rows * 16,
                dpi: 96,
            },
            Arc::new(PropTermConfig),
            "WezTerm",
            "test",
            Box::new(Vec::new()),
        )
    }

    fn make_scrollback_prop_term(rows: usize, cols: usize, scrollback: usize) -> Terminal {
        Terminal::new(
            TerminalSize {
                rows,
                cols,
                pixel_width: cols * 8,
                pixel_height: rows * 16,
                dpi: 96,
            },
            Arc::new(ScrollbackPropTermConfig { scrollback }),
            "WezTerm",
            "test",
            Box::new(Vec::new()),
        )
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn recovery_checkpoint_carries_the_exact_owned_parser_watermark() {
        let mut terminal = make_prop_term(4, 8);
        terminal.advance_bytes(b"abc");
        terminal.advance_bytes(b"defg");

        let checkpoint = terminal
            .capture_recovery_checkpoint(
                crate::terminalstate::checkpoint::TerminalCheckpointLimits::default(),
            )
            .expect("capture owned-parser checkpoint");
        assert_eq!(checkpoint.parser_stream_bytes(), 7);
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn recovery_checkpoint_plaintext_remains_wipe_guarded_when_consumed() {
        fn require_zeroize_on_drop<T: zeroize::ZeroizeOnDrop>() {}
        fn require_zeroizing_payload(_: &Zeroizing<Vec<u8>>) {}

        require_zeroize_on_drop::<RecoveryTerminalCheckpointV2>();
        let checkpoint = make_prop_term(4, 8)
            .capture_recovery_checkpoint(
                crate::terminalstate::checkpoint::TerminalCheckpointLimits::default(),
            )
            .expect("capture wipe-guarded checkpoint");
        let expected = Zeroizing::new(checkpoint.canonical_payload().to_vec());
        let payload = checkpoint.into_canonical_payload();
        require_zeroizing_payload(&payload);
        assert_eq!(payload.as_slice(), expected.as_slice());
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn external_recovery_ground_witness_supplies_the_checkpoint_watermark() {
        let terminal = make_prop_term(4, 8);
        let mut external_parser = Parser::new();
        external_parser.parse(b"external-stream", |_| {});
        let ground = external_parser
            .recovery_ground_boundary()
            .expect("external parser is ground");

        let checkpoint = terminal
            .capture_recovery_checkpoint_at_external_parser_ground(
                ground,
                crate::terminalstate::checkpoint::TerminalCheckpointLimits::default(),
            )
            .expect("capture external-parser checkpoint");
        assert_eq!(checkpoint.parser_stream_bytes(), 15);
    }

    fn snapshot_line(line: &Line) -> LineSnapshot {
        LineSnapshot {
            text: line.as_str().to_string(),
            wrapped: line.last_cell_was_wrapped(),
            cells: line
                .visible_cells()
                .map(|cell| (cell.str().to_string(), cell.width(), cell.attrs().clone()))
                .collect(),
        }
    }

    fn snapshot_term(term: &Terminal) -> TerminalSnapshot {
        let mut cursor = term.cursor_pos();
        cursor.seqno = 0;
        TerminalSnapshot {
            cursor,
            title: term.get_title().to_string(),
            current_dir: term.get_current_dir().map(|url| url.to_string()),
            progress: term.get_progress(),
            palette: term.palette(),
            all_lines: term
                .screen()
                .all_lines()
                .iter()
                .map(snapshot_line)
                .collect(),
        }
    }

    fn chunked_snapshot(payload: &[u8], chunk_sizes: &[usize]) -> TerminalSnapshot {
        let mut term = make_prop_term(8, 16);
        let mut offset = 0;
        for size in chunk_sizes {
            if offset >= payload.len() {
                break;
            }
            let end = (offset + (*size).max(1)).min(payload.len());
            term.advance_bytes(&payload[offset..end]);
            offset = end;
        }
        if offset < payload.len() {
            term.advance_bytes(&payload[offset..]);
        }
        snapshot_term(&term)
    }

    fn single_snapshot(payload: &[u8]) -> TerminalSnapshot {
        let mut term = make_prop_term(8, 16);
        term.advance_bytes(payload);
        snapshot_term(&term)
    }

    fn arb_chunk_sizes() -> impl Strategy<Value = Vec<usize>> {
        proptest::collection::vec(1usize..8, 0..24)
    }

    fn arb_ascii_text() -> impl Strategy<Value = String> {
        proptest::collection::vec(0x20u8..=0x7Eu8, 0..48)
            .prop_map(|bytes| bytes.into_iter().map(char::from).collect())
    }

    fn arb_safe_label() -> impl Strategy<Value = String> {
        proptest::collection::vec(
            prop_oneof![
                (b'A'..=b'Z').prop_map(char::from),
                (b'a'..=b'z').prop_map(char::from),
                (b'0'..=b'9').prop_map(char::from),
                Just(' '),
                Just('_'),
                Just('.'),
                Just('/'),
                Just(':'),
                Just('-'),
            ],
            0..24,
        )
        .prop_map(|chars| chars.into_iter().collect())
    }

    fn arb_multibyte_text() -> impl Strategy<Value = String> {
        proptest::collection::vec(
            prop_oneof![
                Just("a"),
                Just("b"),
                Just(" "),
                Just("\u{00e9}"),
                Just("\u{03bb}"),
                Just("\u{4e2d}"),
                Just("\u{8a9e}"),
                Just("\u{1f980}"),
            ],
            0..32,
        )
        .prop_map(|parts| parts.concat())
    }

    fn arb_control_stream() -> impl Strategy<Value = String> {
        proptest::collection::vec(
            prop_oneof![
                Just("a"),
                Just("b"),
                Just("c"),
                Just("\r"),
                Just("\n"),
                Just("\x08"),
                Just("\t"),
            ],
            0..40,
        )
        .prop_map(|parts| parts.concat())
    }

    #[derive(Debug, Clone)]
    enum PropScrollAction {
        PrintRun(u8, usize),
        CarriageReturn,
        LineFeed,
        ReverseIndex,
        ScrollUp(u8),
        ScrollDown(u8),
        CursorToTop,
        CursorToBottom,
    }

    fn arb_scroll_action() -> impl Strategy<Value = PropScrollAction> {
        prop_oneof![
            (b'A'..=b'Z', 1usize..=24)
                .prop_map(|(byte, len)| { PropScrollAction::PrintRun(byte, len) }),
            Just(PropScrollAction::CarriageReturn),
            Just(PropScrollAction::LineFeed),
            Just(PropScrollAction::ReverseIndex),
            (1u8..=4).prop_map(PropScrollAction::ScrollUp),
            (1u8..=4).prop_map(PropScrollAction::ScrollDown),
            Just(PropScrollAction::CursorToTop),
            Just(PropScrollAction::CursorToBottom),
        ]
    }

    fn arb_scroll_region() -> impl Strategy<Value = (u8, u8)> {
        (1u8..=7).prop_flat_map(|top| (Just(top), (top + 1)..=8))
    }

    fn scrolling_payload(
        auto_wrap: bool,
        top: u8,
        bottom: u8,
        actions: &[PropScrollAction],
    ) -> Vec<u8> {
        let wrap_mode = if auto_wrap { "h" } else { "l" };
        let mut payload = format!("\x1b[?7{wrap_mode}\x1b[{top};{bottom}r").into_bytes();

        for row in 1..=8 {
            let run = if row % 2 == 0 { 20 } else { 12 };
            payload.extend_from_slice(format!("\x1b[{row};1H").as_bytes());
            for _ in 0..run {
                payload.push(b'0' + row);
            }
        }

        payload.extend_from_slice(format!("\x1b[{bottom};1H").as_bytes());
        for action in actions {
            match *action {
                PropScrollAction::PrintRun(byte, len) => {
                    for _ in 0..len {
                        payload.push(byte);
                    }
                }
                PropScrollAction::CarriageReturn => payload.push(b'\r'),
                PropScrollAction::LineFeed => payload.push(b'\n'),
                PropScrollAction::ReverseIndex => payload.extend_from_slice(b"\x1bM"),
                PropScrollAction::ScrollUp(lines) => {
                    payload.extend_from_slice(format!("\x1b[{lines}S").as_bytes());
                }
                PropScrollAction::ScrollDown(lines) => {
                    payload.extend_from_slice(format!("\x1b[{lines}T").as_bytes());
                }
                PropScrollAction::CursorToTop => {
                    payload.extend_from_slice(format!("\x1b[{top};1H").as_bytes());
                }
                PropScrollAction::CursorToBottom => {
                    payload.extend_from_slice(format!("\x1b[{bottom};1H").as_bytes());
                }
            }
        }

        payload
    }

    fn assert_cell_grid_invariants(
        term: &Terminal,
        rows: usize,
        cols: usize,
        auto_wrap: bool,
        top: u8,
        bottom: u8,
        actions: &[PropScrollAction],
    ) -> Result<(), proptest::test_runner::TestCaseError> {
        let cursor = term.cursor_pos();
        prop_assert!(
            cursor.x <= cols,
            "cursor column out of bounds: cursor={cursor:?} cols={cols} auto_wrap={auto_wrap} region={top}..={bottom} actions={actions:?}"
        );
        prop_assert!(
            cursor.y >= 0 && (cursor.y as usize) < rows,
            "cursor row out of bounds: cursor={cursor:?} rows={rows} auto_wrap={auto_wrap} region={top}..={bottom} actions={actions:?}"
        );

        let screen = term.screen();
        let phys_row = screen.phys_row(cursor.y);
        prop_assert!(
            phys_row < screen.scrollback_rows(),
            "cursor phys row {phys_row} outside scrollback rows {} auto_wrap={auto_wrap} region={top}..={bottom} actions={actions:?}",
            screen.scrollback_rows()
        );

        let stable_row = screen.visible_row_to_stable_row(cursor.y);
        let mapped_phys = screen
            .stable_row_to_phys(stable_row)
            .expect("cursor stable row must map to a physical row");
        prop_assert_eq!(
            mapped_phys,
            phys_row,
            "cursor visible/stable/physical row mapping must roundtrip auto_wrap={} region={}..={} actions={:?}",
            auto_wrap,
            top,
            bottom,
            actions
        );

        let all_lines = screen.all_lines();
        prop_assert!(
            all_lines.len() >= rows,
            "screen lost visible rows: all_lines={} rows={rows} auto_wrap={auto_wrap} region={top}..={bottom} actions={actions:?}",
            all_lines.len()
        );
        prop_assert_eq!(
            all_lines.len(),
            screen.scrollback_rows(),
            "screen all_lines and scrollback row count diverged auto_wrap={} region={}..={} actions={:?}",
            auto_wrap,
            top,
            bottom,
            actions
        );

        for (line_idx, line) in all_lines.iter().enumerate() {
            prop_assert!(
                line.len() <= cols,
                "line {line_idx} exceeds terminal width: len={} cols={cols} auto_wrap={auto_wrap} region={top}..={bottom} actions={actions:?}",
                line.len()
            );
            for cell in line.visible_cells() {
                prop_assert!(
                    (1..=cols).contains(&cell.width()),
                    "line {line_idx} has invalid cell width {} auto_wrap={auto_wrap} region={top}..={bottom} actions={actions:?}",
                    cell.width()
                );
            }
        }

        Ok(())
    }

    fn scrollback_marker(index: usize) -> String {
        format!("L{index:04}")
    }

    fn scrollback_eviction_payload(line_count: usize) -> Vec<u8> {
        let mut payload = Vec::new();
        for line_idx in 0..line_count {
            if line_idx > 0 {
                payload.extend_from_slice(b"\r\n");
            }
            payload.extend_from_slice(scrollback_marker(line_idx).as_bytes());
        }
        payload
    }

    fn retained_scrollback_text(term: &Terminal) -> Vec<String> {
        term.screen()
            .all_lines()
            .iter()
            .map(|line| line.as_str().trim_end().to_string())
            .collect()
    }

    #[test]
    fn clipboard_selection_equality() {
        assert_eq!(ClipboardSelection::Clipboard, ClipboardSelection::Clipboard);
        assert_eq!(
            ClipboardSelection::PrimarySelection,
            ClipboardSelection::PrimarySelection
        );
        assert_ne!(
            ClipboardSelection::Clipboard,
            ClipboardSelection::PrimarySelection
        );
    }

    #[test]
    fn clipboard_selection_debug() {
        let dbg = format!("{:?}", ClipboardSelection::Clipboard);
        assert_eq!(dbg, "Clipboard");
    }

    #[test]
    fn clipboard_selection_clone() {
        let sel = ClipboardSelection::PrimarySelection;
        let cloned = sel;
        assert_eq!(sel, cloned);
    }

    #[test]
    fn progress_default_is_none() {
        assert_eq!(Progress::default(), Progress::None);
    }

    #[test]
    fn progress_equality() {
        assert_eq!(Progress::None, Progress::None);
        assert_eq!(Progress::Percentage(50), Progress::Percentage(50));
        assert_ne!(Progress::Percentage(50), Progress::Percentage(75));
        assert_eq!(Progress::Error(1), Progress::Error(1));
        assert_ne!(Progress::Error(1), Progress::Error(2));
        assert_eq!(Progress::Indeterminate, Progress::Indeterminate);
        assert_ne!(Progress::None, Progress::Indeterminate);
    }

    #[test]
    fn progress_clone() {
        let p = Progress::Percentage(42);
        let cloned = p.clone();
        assert_eq!(p, cloned);
    }

    #[test]
    fn progress_debug() {
        assert!(format!("{:?}", Progress::None).contains("None"));
        assert!(format!("{:?}", Progress::Percentage(50)).contains("50"));
        assert!(format!("{:?}", Progress::Error(1)).contains("Error"));
        assert!(format!("{:?}", Progress::Indeterminate).contains("Indeterminate"));
    }

    #[test]
    fn alert_bell() {
        let a = Alert::Bell;
        let b = Alert::Bell;
        assert_eq!(a, b);
    }

    #[test]
    fn alert_toast_notification() {
        let alert = Alert::ToastNotification {
            title: Some("Title".to_string()),
            body: "Body text".to_string(),
            focus: true,
        };
        let alert2 = alert.clone();
        assert_eq!(alert, alert2);
    }

    #[test]
    fn alert_toast_notification_no_title() {
        let alert = Alert::ToastNotification {
            title: None,
            body: "message".to_string(),
            focus: false,
        };
        assert!(matches!(&alert, Alert::ToastNotification { .. }));
        if let Alert::ToastNotification { title, body, focus } = &alert {
            assert!(title.is_none());
            assert_eq!(body, "message");
            assert!(!focus);
        }
    }

    #[test]
    fn alert_variants_inequality() {
        assert_ne!(Alert::Bell, Alert::PaletteChanged);
        assert_ne!(
            Alert::CurrentWorkingDirectoryChanged,
            Alert::OutputSinceFocusLost
        );
    }

    #[test]
    fn alert_set_user_var() {
        let a = Alert::SetUserVar {
            name: "foo".to_string(),
            value: "bar".to_string(),
        };
        let b = Alert::SetUserVar {
            name: "foo".to_string(),
            value: "bar".to_string(),
        };
        assert_eq!(a, b);
    }

    #[test]
    fn alert_progress() {
        let a = Alert::Progress(Progress::Percentage(75));
        let b = Alert::Progress(Progress::Percentage(75));
        assert_eq!(a, b);
        assert_ne!(a, Alert::Progress(Progress::None));
    }

    #[test]
    fn alert_window_title_changed() {
        let a = Alert::WindowTitleChanged("hello".to_string());
        let b = Alert::WindowTitleChanged("hello".to_string());
        assert_eq!(a, b);
        assert_ne!(a, Alert::WindowTitleChanged("world".to_string()));
    }

    #[test]
    fn alert_icon_title_changed() {
        let a = Alert::IconTitleChanged(Some("icon".to_string()));
        let b = Alert::IconTitleChanged(None);
        assert_ne!(a, b);
    }

    #[test]
    fn alert_tab_title_changed() {
        let a = Alert::TabTitleChanged(Some("tab".to_string()));
        let b = Alert::TabTitleChanged(Some("tab".to_string()));
        assert_eq!(a, b);
    }

    #[test]
    fn terminal_size_default() {
        let size = TerminalSize::default();
        assert_eq!(size.rows, 24);
        assert_eq!(size.cols, 80);
        assert_eq!(size.pixel_width, 0);
        assert_eq!(size.pixel_height, 0);
        assert_eq!(size.dpi, 0);
    }

    #[test]
    fn terminal_size_equality() {
        let a = TerminalSize::default();
        let b = TerminalSize::default();
        assert_eq!(a, b);
    }

    #[test]
    fn terminal_size_inequality() {
        let a = TerminalSize::default();
        let b = TerminalSize {
            rows: 25,
            ..TerminalSize::default()
        };
        assert_ne!(a, b);
    }

    #[test]
    fn terminal_size_clone_and_copy() {
        let a = TerminalSize {
            rows: 40,
            cols: 120,
            pixel_width: 960,
            pixel_height: 640,
            dpi: 96,
        };
        let b = a; // Copy
        #[allow(clippy::clone_on_copy)]
        let c = a.clone(); // Clone
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn terminal_size_debug() {
        let size = TerminalSize::default();
        let dbg = format!("{:?}", size);
        assert!(dbg.contains("TerminalSize"));
        assert!(dbg.contains("24"));
        assert!(dbg.contains("80"));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(24))]

        #[test]
        fn incremental_ascii_text_matches_single_shot(
            text in arb_ascii_text(),
            chunk_sizes in arb_chunk_sizes(),
        ) {
            let payload = text.into_bytes();
            prop_assert_eq!(single_snapshot(&payload), chunked_snapshot(&payload, &chunk_sizes));
        }

        #[test]
        fn incremental_multibyte_text_matches_single_shot(
            text in arb_multibyte_text(),
            chunk_sizes in arb_chunk_sizes(),
        ) {
            let payload = text.into_bytes();
            prop_assert_eq!(single_snapshot(&payload), chunked_snapshot(&payload, &chunk_sizes));
        }

        #[test]
        fn incremental_control_stream_matches_single_shot(
            text in arb_control_stream(),
            chunk_sizes in arb_chunk_sizes(),
        ) {
            let payload = text.into_bytes();
            prop_assert_eq!(single_snapshot(&payload), chunked_snapshot(&payload, &chunk_sizes));
        }

        #[test]
        fn incremental_cursor_csi_sequences_match_single_shot(
            row in 1u8..=6,
            col in 1u8..=12,
            right in 1u8..=6,
            left in 1u8..=6,
            text in arb_ascii_text(),
            chunk_sizes in arb_chunk_sizes(),
        ) {
            let payload = format!(
                "home\x1b[{row};{col}H{text}\x1b[{right}C>\x1b[{left}D<"
            ).into_bytes();
            prop_assert_eq!(single_snapshot(&payload), chunked_snapshot(&payload, &chunk_sizes));
        }

        #[test]
        fn incremental_sgr_palette_sequences_match_single_shot(
            fg in 30u8..=37,
            bg in 40u8..=47,
            text in arb_ascii_text(),
            chunk_sizes in arb_chunk_sizes(),
        ) {
            let payload = format!("\x1b[{fg};{bg};1m{text}Z\x1b[0m!").into_bytes();
            prop_assert_eq!(single_snapshot(&payload), chunked_snapshot(&payload, &chunk_sizes));
        }

        #[test]
        fn incremental_sgr_truecolor_sequences_match_single_shot(
            fg_r in any::<u8>(),
            fg_g in any::<u8>(),
            fg_b in any::<u8>(),
            bg_r in any::<u8>(),
            bg_g in any::<u8>(),
            bg_b in any::<u8>(),
            text in arb_ascii_text(),
            chunk_sizes in arb_chunk_sizes(),
        ) {
            let payload = format!(
                "\x1b[38;2;{fg_r};{fg_g};{fg_b};48;2;{bg_r};{bg_g};{bg_b}m{text}Q\x1b[0m!"
            ).into_bytes();
            prop_assert_eq!(single_snapshot(&payload), chunked_snapshot(&payload, &chunk_sizes));
        }

        #[test]
        fn incremental_osc_title_st_sequences_match_single_shot(
            title in arb_safe_label(),
            body in arb_ascii_text(),
            chunk_sizes in arb_chunk_sizes(),
        ) {
            let payload = format!("\x1b]0;{title}\x1b\\{body}").into_bytes();
            prop_assert_eq!(single_snapshot(&payload), chunked_snapshot(&payload, &chunk_sizes));
        }

        #[test]
        fn incremental_osc_title_bel_sequences_match_single_shot(
            title in arb_safe_label(),
            body in arb_ascii_text(),
            chunk_sizes in arb_chunk_sizes(),
        ) {
            let payload = format!("\x1b]2;{title}\x07{body}").into_bytes();
            prop_assert_eq!(single_snapshot(&payload), chunked_snapshot(&payload, &chunk_sizes));
        }

        #[test]
        fn incremental_osc_palette_change_sequences_match_single_shot(
            index in any::<u8>(),
            red in any::<u8>(),
            green in any::<u8>(),
            blue in any::<u8>(),
            chunk_sizes in arb_chunk_sizes(),
        ) {
            let payload = format!(
                "\x1b]4;{index};rgb:{red:02x}/{green:02x}/{blue:02x}\x1b\\X"
            ).into_bytes();
            prop_assert_eq!(single_snapshot(&payload), chunked_snapshot(&payload, &chunk_sizes));
        }

        #[test]
        fn incremental_osc_palette_reset_sequences_match_single_shot(
            index in any::<u8>(),
            red in any::<u8>(),
            green in any::<u8>(),
            blue in any::<u8>(),
            chunk_sizes in arb_chunk_sizes(),
        ) {
            let payload = format!(
                "\x1b]4;{index};rgb:{red:02x}/{green:02x}/{blue:02x}\x1b\\\x1b]104;{index}\x1b\\X"
            ).into_bytes();
            prop_assert_eq!(single_snapshot(&payload), chunked_snapshot(&payload, &chunk_sizes));
        }

        #[test]
        fn incremental_osc_dynamic_color_sequences_match_single_shot(
            fg_r in any::<u8>(),
            fg_g in any::<u8>(),
            fg_b in any::<u8>(),
            bg_r in any::<u8>(),
            bg_g in any::<u8>(),
            bg_b in any::<u8>(),
            cursor_r in any::<u8>(),
            cursor_g in any::<u8>(),
            cursor_b in any::<u8>(),
            chunk_sizes in arb_chunk_sizes(),
        ) {
            let payload = format!(
                "\x1b]10;rgb:{fg_r:02x}/{fg_g:02x}/{fg_b:02x}\x1b\\\
                 \x1b]11;rgb:{bg_r:02x}/{bg_g:02x}/{bg_b:02x}\x1b\\\
                 \x1b]12;rgb:{cursor_r:02x}/{cursor_g:02x}/{cursor_b:02x}\x1b\\X"
            ).into_bytes();
            prop_assert_eq!(single_snapshot(&payload), chunked_snapshot(&payload, &chunk_sizes));
        }

        #[test]
        fn incremental_mixed_escape_stream_matches_single_shot(
            title in arb_safe_label(),
            body in arb_multibyte_text(),
            index in any::<u8>(),
            red in any::<u8>(),
            green in any::<u8>(),
            blue in any::<u8>(),
            row in 1u8..=6,
            col in 1u8..=12,
            chunk_sizes in arb_chunk_sizes(),
        ) {
            let payload = format!(
                "\x1b]0;{title}\x1b\\{body}\n\
                 \x1b[31;47mZ\x1b[0m\
                 \x1b]4;{index};rgb:{red:02x}/{green:02x}/{blue:02x}\x1b\\\
                 \x1b[{row};{col}H\u{4e2d}"
            ).into_bytes();
            prop_assert_eq!(single_snapshot(&payload), chunked_snapshot(&payload, &chunk_sizes));
        }

        #[test]
        fn scrolling_cell_grid_invariants_hold_under_all_line_wrap_modes(
            (top, bottom) in arb_scroll_region(),
            actions in proptest::collection::vec(arb_scroll_action(), 0..32),
            chunk_sizes in arb_chunk_sizes(),
        ) {
            for auto_wrap in [false, true] {
                let payload = scrolling_payload(auto_wrap, top, bottom, &actions);
                prop_assert_eq!(
                    single_snapshot(&payload),
                    chunked_snapshot(&payload, &chunk_sizes),
                    "chunked parse changed scrolling grid auto_wrap={} region={}..={} actions={:?}",
                    auto_wrap,
                    top,
                    bottom,
                    actions
                );

                let mut term = make_prop_term(8, 16);
                term.advance_bytes(&payload);
                assert_cell_grid_invariants(&term, 8, 16, auto_wrap, top, bottom, &actions)?;
            }
        }

        #[test]
        fn scrollback_eviction_keeps_latest_visible_rows_and_bounded_retention(
            rows in 2usize..=8,
            cols in 8usize..=24,
            scrollback in 0usize..=32,
            overflow_lines in 1usize..=96,
        ) {
            let line_count = rows + scrollback + overflow_lines;
            let payload = scrollback_eviction_payload(line_count);
            let mut term = make_scrollback_prop_term(rows, cols, scrollback);
            term.advance_bytes(&payload);

            let retained = retained_scrollback_text(&term);
            let expected_capacity = rows + scrollback;
            prop_assert!(
                retained.len() <= expected_capacity,
                "scrollback retained {} rows beyond capacity {} rows={rows} cols={cols} scrollback={scrollback} line_count={line_count}",
                retained.len(),
                expected_capacity,
            );
            prop_assert!(
                retained.len() >= rows,
                "scrollback retained fewer than visible rows: retained={} rows={rows} scrollback={scrollback} line_count={line_count}",
                retained.len(),
            );
            prop_assert_eq!(
                retained.len(),
                term.screen().scrollback_rows(),
                "line snapshot count diverged from screen row count rows={} cols={} scrollback={} line_count={}",
                rows,
                cols,
                scrollback,
                line_count,
            );

            let oldest_marker = scrollback_marker(0);
            prop_assert!(
                !retained.iter().any(|line| line.contains(&oldest_marker)),
                "oldest marker was retained after overflow rows={rows} cols={cols} scrollback={scrollback} line_count={line_count} retained={retained:?}",
            );

            let visible_tail_start = line_count - rows;
            let visible_tail = &retained[retained.len() - rows..];
            for (offset, actual) in visible_tail.iter().enumerate() {
                let expected_marker = scrollback_marker(visible_tail_start + offset);
                prop_assert!(
                    actual.contains(&expected_marker),
                    "visible row {offset} lost latest marker {expected_marker}: actual={actual:?} rows={rows} cols={cols} scrollback={scrollback} line_count={line_count} retained={retained:?}",
                );
            }

            let cursor = term.cursor_pos();
            prop_assert!(
                cursor.x <= cols,
                "cursor column escaped after eviction: cursor={cursor:?} cols={cols} rows={rows} scrollback={scrollback} line_count={line_count}",
            );
            prop_assert!(
                cursor.y >= 0 && (cursor.y as usize) < rows,
                "cursor row escaped after eviction: cursor={cursor:?} rows={rows} cols={cols} scrollback={scrollback} line_count={line_count}",
            );
            let phys_row = term.screen().phys_row(cursor.y);
            prop_assert!(
                phys_row < term.screen().scrollback_rows(),
                "cursor physical row {phys_row} escaped retained rows {} after eviction rows={rows} cols={cols} scrollback={scrollback} line_count={line_count}",
                term.screen().scrollback_rows(),
            );
        }
    }
}
