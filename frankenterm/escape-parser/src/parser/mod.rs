#![allow(clippy::many_single_char_names)]
use crate::csi::{Sgr, decode_common_csi, decode_sgr};
#[cfg(feature = "tmux_cc")]
use crate::tmux_cc::Event;
use crate::{
    Action, CSI, ControlCode, DeviceControlMode, EnterDeviceControlMode, Esc,
    OperatingSystemCommand, ShortDeviceControl,
};
#[cfg(feature = "tmux_cc")]
use core::borrow::BorrowMut;
use core::cell::RefCell;
use log::error;
use num_traits::FromPrimitive;
use vtparse::{CsiParam, VTActor, VTParser};

use crate::allocate::*;

mod ascii;
mod sixel;
pub use ascii::AsciiScan;
use sixel::SixelBuilder;

const MAX_TCAP_NAMES: usize = 512;
const MAX_TCAP_CURRENT_BYTES: usize = 1_048_576;
// Bound the aggregate retained capability-name input as well as each name.
// Without a request-wide budget, 512 individually valid near-limit names could
// retain hundreds of MiB before the DCS terminator arrived.
const MAX_TCAP_TOTAL_BYTES: usize = 1_048_576;

/// Set-wide escape hatch for the Round-7 moonshot-recommended default-on set.
/// Falsey values disable promoted recommended gates while leaving unrelated
/// moonshots alone.
#[cfg(feature = "std")]
const MOONSHOT_RECOMMENDED_ENV: &str = "FT_MOONSHOT_RECOMMENDED";

/// Env gate for the Round-5 D1 printable-run batching optimization
/// (ft-round5-gauntlet-lw0s7.10). Round-7 promotes this to default-ON as part
/// of the dense-ASCII term-render stack; falsey values disable it for newly
/// constructed parsers.
#[cfg(feature = "std")]
const PRINT_BATCHING_ENV: &str = "FT_MOONSHOT_PARSER_PRINT_BATCHING";

/// Resolve the default `print_batching` setting for a freshly constructed
/// [`Parser`]. On unless `FT_MOONSHOT_RECOMMENDED` or
/// `FT_MOONSHOT_PARSER_PRINT_BATCHING` is set falsey. The env read is
/// `std`-only; `no_std` builds use the promoted default.
#[inline]
fn default_print_batching() -> bool {
    #[cfg(feature = "std")]
    {
        let recommended = std::env::var(MOONSHOT_RECOMMENDED_ENV).ok();
        let print_batching = std::env::var(PRINT_BATCHING_ENV).ok();
        return print_batching_default_for_values(
            recommended.as_deref(),
            print_batching.as_deref(),
        );
    }
    #[cfg(not(feature = "std"))]
    print_batching_default_for_values(None, None)
}

/// Pure policy core for [`default_print_batching`]. Keeping environment I/O at
/// the boundary lets the full default/escape-hatch matrix be tested without
/// mutating process-global environment variables in a parallel test suite.
#[inline]
fn print_batching_default_for_values(
    recommended: Option<&str>,
    print_batching: Option<&str>,
) -> bool {
    !recommended.is_some_and(env_value_is_falsey)
        && !print_batching.is_some_and(env_value_is_falsey)
}

/// Env gate for the Round-5 D2 table-driven CSI/OSC dispatch optimization
/// (ft-round5-gauntlet-lw0s7.12). Default-OFF; see [`default_table_dispatch`].
const TABLE_DISPATCH_ENV: &str = "FT_MOONSHOT_PARSER_TABLE_DISPATCH";

/// Resolve the default `table_dispatch` setting for a freshly constructed
/// [`Parser`]. Off unless the `parser-table-dispatch` feature is compiled in or
/// the `FT_MOONSHOT_PARSER_TABLE_DISPATCH` env var is set truthy at run time.
#[inline]
fn default_table_dispatch() -> bool {
    cfg!(feature = "parser-table-dispatch") || env_flag_truthy(TABLE_DISPATCH_ENV)
}

/// Shared truthy-env reader for the `FT_MOONSHOT_*` parser gates (mirrors the
/// sibling storage/scrollback gates). The env read is `std`-only; `no_std`
/// builds rely on the compile-time feature flags alone and always return false.
#[inline]
fn env_flag_truthy(_name: &str) -> bool {
    #[cfg(feature = "std")]
    {
        if let Ok(value) = std::env::var(_name) {
            let value = value.trim();
            return value.eq_ignore_ascii_case("1")
                || value.eq_ignore_ascii_case("on")
                || value.eq_ignore_ascii_case("true")
                || value.eq_ignore_ascii_case("yes");
        }
    }
    false
}

/// Kill switch for the CSI fast path (ft-yccm0.3.2.4): a falsey value turns
/// it off for newly constructed parsers. See [`Parser::set_csi_fast_path`].
#[cfg(feature = "std")]
const CSI_FAST_PATH_ENV: &str = "FT_CSI_FAST_PATH";

/// Resolve the default `csi_fast_path` setting for a freshly constructed
/// [`Parser`]: on unless `FT_CSI_FAST_PATH` is set falsey. `no_std` builds
/// use the default.
#[inline]
fn default_csi_fast_path() -> bool {
    #[cfg(feature = "std")]
    {
        let value = std::env::var(CSI_FAST_PATH_ENV).ok();
        return csi_fast_path_default_for_value(value.as_deref());
    }
    #[cfg(not(feature = "std"))]
    csi_fast_path_default_for_value(None)
}

/// Kill switch and width choice for the ground-state ASCII scan
/// (ft-yccm0.3.2.2): falsey selects the scalar scan for newly constructed
/// parsers, and `16`, `32` or `64` a `std::simd` width. See
/// [`AsciiScan::for_env_value`] and [`Parser::set_ascii_scan`].
#[cfg(feature = "std")]
const PARSER_SIMD_ENV: &str = "FT_PARSER_SIMD";

/// Resolve the default [`AsciiScan`] for a freshly constructed [`Parser`].
/// `no_std` builds use [`AsciiScan::DEFAULT`].
#[inline]
fn default_ascii_scan() -> AsciiScan {
    #[cfg(feature = "std")]
    {
        let value = std::env::var(PARSER_SIMD_ENV).ok();
        return AsciiScan::for_env_value(value.as_deref());
    }
    #[cfg(not(feature = "std"))]
    AsciiScan::for_env_value(None)
}

/// Kill switch for validating a printable run's UTF-8 in one pass
/// (ft-yccm0.3.2.3): a falsey value makes newly constructed parsers check
/// each character on its own, the oracle. See [`Parser::set_simd_utf8`].
#[cfg(feature = "std")]
const PARSER_SIMD_UTF8_ENV: &str = "FT_PARSER_SIMD_UTF8";

/// Resolve the default `simd_utf8` setting for a freshly constructed
/// [`Parser`]: on unless `FT_PARSER_SIMD_UTF8` is set falsey.
#[inline]
fn default_simd_utf8() -> bool {
    #[cfg(feature = "std")]
    {
        let value = std::env::var(PARSER_SIMD_UTF8_ENV).ok();
        return csi_fast_path_default_for_value(value.as_deref());
    }
    #[cfg(not(feature = "std"))]
    csi_fast_path_default_for_value(None)
}

/// Pure policy core for [`default_csi_fast_path`] and
/// [`default_simd_utf8`], testable without touching the process
/// environment: on unless the value is falsey.
#[inline]
fn csi_fast_path_default_for_value(value: Option<&str>) -> bool {
    !value.is_some_and(env_value_is_falsey)
}

#[inline]
fn env_value_is_falsey(value: &str) -> bool {
    let value = value.trim();
    value.is_empty()
        || value == "0"
        || value.eq_ignore_ascii_case("false")
        || value.eq_ignore_ascii_case("off")
        || value.eq_ignore_ascii_case("no")
}

struct GetTcapBuilder {
    current: Vec<u8>,
    names: Vec<String>,
    accepted_raw_name_bytes: usize,
    max_current_bytes: usize,
    max_total_bytes: usize,
    discarding_all: bool,
    rejected: bool,
    pending_sequence_error: Option<crate::StringSequenceError>,
}

impl GetTcapBuilder {
    fn new(max_string_sequence_bytes: usize) -> Self {
        Self {
            current: Vec::new(),
            names: Vec::new(),
            accepted_raw_name_bytes: 0,
            max_current_bytes: max_string_sequence_bytes.min(MAX_TCAP_CURRENT_BYTES),
            max_total_bytes: max_string_sequence_bytes.min(MAX_TCAP_TOTAL_BYTES),
            discarding_all: false,
            rejected: false,
            pending_sequence_error: None,
        }
    }

    fn record_sequence_error(&mut self, error: crate::StringSequenceError) {
        self.rejected = true;
        self.discarding_all = true;
        self.current.clear();
        self.names.clear();
        self.accepted_raw_name_bytes = 0;
        if self.pending_sequence_error.is_none() {
            self.pending_sequence_error = Some(error);
        }
    }

    fn take_sequence_error(&mut self) -> Option<crate::StringSequenceError> {
        self.pending_sequence_error.take()
    }

    fn flush(&mut self) {
        if self.discarding_all {
            self.current.clear();
            return;
        }
        if self.current.is_empty() {
            return;
        }
        if self.names.len() >= MAX_TCAP_NAMES {
            log::warn!(
                "XtGetTcap names exceeded {} limit; discarding further names",
                MAX_TCAP_NAMES,
            );
            self.record_sequence_error(crate::StringSequenceError::LimitExceeded {
                kind: crate::StringSequenceKind::XtGetTcap,
                maximum: MAX_TCAP_NAMES,
            });
            self.discarding_all = true;
            self.current.clear();
            return;
        }
        let Some(next_total) = self.accepted_raw_name_bytes.checked_add(self.current.len()) else {
            log::warn!(
                "XtGetTcap aggregate name input overflowed its byte counter; discarding further names"
            );
            self.record_sequence_error(crate::StringSequenceError::LimitExceeded {
                kind: crate::StringSequenceKind::XtGetTcap,
                maximum: self.max_total_bytes,
            });
            self.discarding_all = true;
            self.current.clear();
            return;
        };
        if next_total > self.max_total_bytes {
            log::warn!(
                "XtGetTcap aggregate name input exceeded {} byte limit; discarding the current and further names",
                self.max_total_bytes,
            );
            self.record_sequence_error(crate::StringSequenceError::LimitExceeded {
                kind: crate::StringSequenceKind::XtGetTcap,
                maximum: self.max_total_bytes,
            });
            self.discarding_all = true;
            self.current.clear();
            return;
        }
        let decoded = hex::decode(&self.current)
            .map(|s| String::from_utf8_lossy(&s).to_string())
            .unwrap_or_else(|_| String::from_utf8_lossy(&self.current).to_string());
        if self.names.try_reserve(1).is_err() {
            self.record_sequence_error(crate::StringSequenceError::AllocationFailed {
                kind: crate::StringSequenceKind::XtGetTcap,
            });
            self.discarding_all = true;
            self.current.clear();
            return;
        }
        self.names.push(decoded);
        self.accepted_raw_name_bytes = next_total;
        self.current.clear();
    }

    pub fn push(&mut self, data: u8) {
        if data == b';' {
            self.flush();
        } else if !self.discarding_all {
            if self.current.len() >= self.max_current_bytes {
                log::warn!(
                    "XtGetTcap name exceeded {} byte limit; discarding the whole request",
                    self.max_current_bytes,
                );
                self.record_sequence_error(crate::StringSequenceError::LimitExceeded {
                    kind: crate::StringSequenceKind::XtGetTcap,
                    maximum: self.max_current_bytes,
                });
            } else if self
                .accepted_raw_name_bytes
                .checked_add(self.current.len())
                .is_none_or(|retained| retained >= self.max_total_bytes)
            {
                log::warn!(
                    "XtGetTcap aggregate name input exceeded {} byte limit; discarding the current and further names",
                    self.max_total_bytes,
                );
                self.record_sequence_error(crate::StringSequenceError::LimitExceeded {
                    kind: crate::StringSequenceKind::XtGetTcap,
                    maximum: self.max_total_bytes,
                });
                self.current.clear();
                self.discarding_all = true;
            } else if self.current.try_reserve(1).is_err() {
                self.record_sequence_error(crate::StringSequenceError::AllocationFailed {
                    kind: crate::StringSequenceKind::XtGetTcap,
                });
                self.current.clear();
                self.discarding_all = true;
            } else {
                self.current.push(data);
            }
        }
    }

    pub fn finish(mut self) -> (Option<Vec<String>>, Option<crate::StringSequenceError>) {
        self.flush();
        let names = (!self.rejected).then_some(self.names);
        (names, self.pending_sequence_error)
    }
}

#[derive(Default)]
struct ParseState {
    sixel: Option<SixelBuilder>,
    dcs: Option<ShortDeviceControl>,
    discarding_short_dcs: bool,
    get_tcap: Option<GetTcapBuilder>,
    #[cfg(feature = "tmux_cc")]
    tmux_state: Option<RefCell<crate::tmux_cc::Parser>>,
    pending_string_sequence_error: Option<crate::StringSequenceError>,
    max_string_sequence_bytes: usize,
    /// Round-5 D2 (ft-round5-gauntlet-lw0s7.12): when set, [`Performer`]'s
    /// CSI/OSC dispatch uses the table-driven fast decoders ([`CSI::parse_fast`]
    /// / [`OperatingSystemCommand::parse_fast`]) before the generic parser.
    /// Default-OFF; lives on `ParseState` because the `VTActor` dispatch methods
    /// (`csi_dispatch`/`osc_dispatch`) only have access to the parse state.
    table_dispatch: bool,
}

const MAX_SHORT_DCS_BYTES: usize = 8 * 1024 * 1024;

/// Receives what [`Parser::parse_with`] decodes, one call per action, in
/// stream order (ft-yccm0.3.2.1). The parser is generic over the handler, so
/// every call is static dispatch, and nothing is collected: a handler that
/// applies each action as it arrives needs no `Vec<Action>`.
///
/// Only [`Handler::action`] is required. The other methods carry the hot
/// actions without building an [`Action`]; a printable run arrives as a
/// borrowed `&str` rather than an owned `String`. Their defaults forward
/// exactly the `Action` that [`Parser::parse`] produces, so overriding none
/// of them reproduces the `Action` stream.
pub trait Handler {
    /// Every action the other methods do not take themselves.
    fn action(&mut self, action: Action);

    /// [`Action::Print`].
    #[inline]
    fn print(&mut self, c: char) {
        self.action(Action::Print(c));
    }

    /// [`Action::PrintString`]: a run of ground-state printable characters.
    #[inline]
    fn print_str(&mut self, text: &str) {
        self.action(Action::PrintString(text.to_string()));
    }

    /// [`Action::PrintString`] for a run that is all printable ASCII
    /// (`0x20..=0x7e`) and at least two bytes long (ft-yccm0.3.2.2). A
    /// handler that knows each byte is one narrow cell can write the run
    /// straight into a row; the default hands it to [`Handler::print_str`].
    #[inline]
    fn print_ascii_run(&mut self, run: &str) {
        self.print_str(run);
    }

    /// [`Action::Control`].
    #[inline]
    fn control(&mut self, code: ControlCode) {
        self.action(Action::Control(code));
    }

    /// [`Action::CSI`].
    #[inline]
    fn csi(&mut self, csi: CSI) {
        self.action(Action::CSI(csi));
    }

    /// [`Action::Esc`].
    #[inline]
    fn esc(&mut self, esc: Esc) {
        self.action(Action::Esc(esc));
    }

    /// One [`Action::CSI`]`(`[`CSI::Sgr`]`)` per setting, in order. The CSI
    /// fast path (ft-yccm0.3.2.4) hands over the settings of consecutive SGR
    /// sequences in one call; nothing lies between them, so nothing can
    /// observe the pen in between.
    #[inline]
    fn sgr(&mut self, sgrs: &[Sgr]) {
        for sgr in sgrs {
            self.csi(CSI::Sgr(sgr.clone()));
        }
    }
}

/// The [`Handler`] for consumers that want [`Action`] values (the mux codec,
/// the recorder, scripting, tests): every action reaches the closure as the
/// `Action` that [`Parser::parse`] has always produced.
pub struct ActionCollector<F: FnMut(Action)>(pub F);

impl<F: FnMut(Action)> Handler for ActionCollector<F> {
    #[inline]
    fn action(&mut self, action: Action) {
        (self.0)(action)
    }
}

/// The `Parser` struct holds the state machine that is used to decode
/// a sequence of bytes.  The byte sequence can be streaming into the
/// state machine.
/// You can either have the parser trigger a callback as `Action`s are
/// decoded, or have it return a `Vec<Action>` holding zero-or-more
/// decoded actions.
pub struct Parser {
    state_machine: VTParser,
    state: RefCell<ParseState>,
    /// Actions emitted before parse_first_as_vec reaches a ground boundary.
    pending_actions: alloc::collections::VecDeque<Action>,
    pending_sequence_bytes: usize,
    /// Total raw bytes accepted by this parser's streaming API. `None` means
    /// the position overflowed and can no longer authorize a durable recovery
    /// boundary.  The parser remains usable for ordinary terminal rendering in
    /// that case, but checkpoint admission must fail closed.
    recovery_stream_bytes: Option<u64>,
    /// Round-5 D1 (ft-round5-gauntlet-lw0s7.10): when set, [`Parser::parse`]
    /// coalesces maximal ground-state printable UTF-8 runs into a single
    /// `Action::PrintString` instead of emitting one `Action::Print(char)` per
    /// codepoint. The promoted default is on; a falsey
    /// `FT_MOONSHOT_RECOMMENDED` or `FT_MOONSHOT_PARSER_PRINT_BATCHING` value
    /// disables it for newly constructed parsers, and
    /// [`Parser::set_print_batching`] overrides the resolved default.
    print_batching: bool,
    /// The CSI fast path (ft-yccm0.3.2.4); see [`Parser::set_csi_fast_path`].
    csi_fast_path: bool,
    /// The ground-state ASCII scan (ft-yccm0.3.2.2); see
    /// [`Parser::set_ascii_scan`].
    ascii_scan: AsciiScan,
    /// One UTF-8 validation per printable run (ft-yccm0.3.2.3); see
    /// [`Parser::set_simd_utf8`].
    simd_utf8: bool,
    /// The fast path's parameter array, reused for every sequence: fixed
    /// size, never on the heap, and never cleared (only the prefix a scan
    /// writes is read).
    csi_params: [CsiParam; CSI_FAST_MAX_PARAMS],
    /// The settings of the SGR sequences the fast path is decoding, handed
    /// to [`Handler::sgr`] together.
    sgr_run: [Sgr; SGR_RUN_MAX],
    last_sgr: LastSgr,
}

/// Parameters the CSI fast path holds. A sequence with more goes through
/// the state machine.
const CSI_FAST_MAX_PARAMS: usize = 32;

/// SGR settings one [`Handler::sgr`] call carries at most. One sequence of
/// [`CSI_FAST_MAX_PARAMS`] parameters decodes to at most half of that plus
/// one, so it always fits once the run is handed over.
const SGR_RUN_MAX: usize = 32;

/// Longest SGR parameter string the last-SGR cache keeps.
const LAST_SGR_KEY_MAX: usize = 24;

/// Most settings the last-SGR cache keeps.
const LAST_SGR_MAX: usize = 4;

/// The one-entry last-SGR cache (ft-yccm0.3.2.4): the parameter bytes of the
/// last SGR sequence the fast path decoded, and its settings. A sequence
/// with the same bytes is answered from here, with no parameter scan and no
/// decode. The settings are a pure function of those bytes, so a hit is
/// never stale.
///
/// The pen is a plain value that each setting updates in place, with no
/// style set to hash or probe as Ghostty has, so decoding is the work a hit
/// saves.
struct LastSgr {
    key: [u8; LAST_SGR_KEY_MAX],
    key_len: usize,
    sgrs: [Sgr; LAST_SGR_MAX],
    /// The number of settings held; 0 while the cache is empty.
    count: usize,
}

impl LastSgr {
    fn new() -> Self {
        Self {
            key: [0; LAST_SGR_KEY_MAX],
            key_len: 0,
            sgrs: core::array::from_fn(|_| Sgr::Reset),
            count: 0,
        }
    }

    /// Given the bytes after `ESC[`, the length of the cached parameters
    /// plus the `m` when those bytes start with them.
    #[inline]
    fn hit(&self, body: &[u8]) -> Option<usize> {
        if self.count == 0 {
            return None;
        }
        let len = self.key_len;
        let sequence = body.get(..=len)?;
        (sequence[len] == b'm' && sequence[..len] == self.key[..len]).then_some(len + 1)
    }

    /// Remembers `sgrs` as what the parameter bytes `key` decode to. An
    /// entry too large to keep leaves the cache as it was.
    #[inline]
    fn store(&mut self, key: &[u8], sgrs: &[Sgr]) {
        if key.len() > LAST_SGR_KEY_MAX || sgrs.len() > LAST_SGR_MAX {
            return;
        }
        self.key[..key.len()].copy_from_slice(key);
        self.key_len = key.len();
        self.sgrs[..sgrs.len()].clone_from_slice(sgrs);
        self.count = sgrs.len();
    }
}

/// Bounded partial sequence; no emitted actions are discarded. The parser
/// remains at the reported byte boundary and callers may resume or recover.
pub struct FirstSequenceLimitExceeded {
    /// Actions already emitted, in input order; caller owns their delivery.
    pub actions: Vec<Action>,
    /// Bytes consumed from this call only; retry from this offset to resume.
    pub consumed: usize,
}

impl core::fmt::Debug for FirstSequenceLimitExceeded {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("FirstSequenceLimitExceeded")
            .field("action_count", &self.actions.len())
            .field("consumed", &self.consumed)
            .finish()
    }
}

impl core::fmt::Display for FirstSequenceLimitExceeded {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str("first sequence collection limit exceeded")
    }
}

impl core::error::Error for FirstSequenceLimitExceeded {}

/// Maximum emitted actions retained while waiting for a ground boundary.
/// A single byte can emit several actions; an over-limit atomic dispatch is
/// handed back in full through `FirstSequenceLimitExceeded`, never retained.
pub const MAX_FIRST_SEQUENCE_ACTIONS: usize = 1024;
/// Maximum raw sequence bytes consumed across chunks before explicit handoff.
pub const MAX_FIRST_SEQUENCE_BYTES: usize = 1_048_576;

/// Non-constructible witness that one exact parser was recovery-ground after
/// consuming [`Self::stream_bytes`] raw bytes.
///
/// The witness borrows the parser, so that parser cannot consume later bytes
/// until the caller has finished the boundary operation.  Durable mux capture
/// must additionally fence delivery at the same byte watermark and bind the
/// witness to the corresponding authenticated output-journal receipt; this
/// type proves parser state, not journal durability by itself.
#[must_use = "a recovery-ground witness must be consumed by the exact boundary operation"]
pub struct RecoveryGroundBoundary<'parser> {
    _parser: &'parser Parser,
    stream_bytes: u64,
}

impl RecoveryGroundBoundary<'_> {
    /// Cumulative raw bytes consumed by the witnessed parser incarnation.
    #[must_use]
    pub const fn stream_bytes(&self) -> u64 {
        self.stream_bytes
    }
}

impl core::fmt::Debug for RecoveryGroundBoundary<'_> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("RecoveryGroundBoundary")
            .field("stream_bytes", &self.stream_bytes)
            .finish_non_exhaustive()
    }
}

/// Versioned identity for the parser state contract used by durable terminal
/// checkpoints.
///
/// A checkpoint may be restored with a fresh parser only when it was captured
/// with a [`Parser::recovery_ground_boundary`] witness and carries this exact
/// identity. Any change that alters the interpretation of retained raw
/// terminal bytes must change this value so an older checkpoint fails closed
/// instead of silently reconstructing different terminal state.
pub const RECOVERY_CHECKPOINT_PARSER_ID: &str = "frankenterm.escape-parser.recovery-ground.v3";

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

impl Parser {
    pub fn new() -> Self {
        Self::new_with_max_string_sequence_bytes(crate::DEFAULT_MAX_STRING_SEQUENCE_BYTES)
    }

    /// Construct a streaming parser with an explicit retained-payload limit.
    /// OSC and APC use this limit directly; Sixel, short DCS, XtGetTcap, and
    /// tmux control mode use the smaller of this limit and their own
    /// protocol-specific hard cap. The parser drops the whole over-limit
    /// command and exposes a sticky content-free failure through
    /// [`Self::take_string_sequence_error`].
    pub fn new_with_max_string_sequence_bytes(max_string_sequence_bytes: usize) -> Self {
        let state = ParseState {
            table_dispatch: default_table_dispatch(),
            max_string_sequence_bytes,
            ..Default::default()
        };
        Self {
            state_machine: VTParser::new_with_max_string_sequence_bytes(max_string_sequence_bytes),
            state: RefCell::new(state),
            pending_actions: alloc::collections::VecDeque::new(),
            pending_sequence_bytes: 0,
            recovery_stream_bytes: Some(0),
            print_batching: default_print_batching(),
            csi_fast_path: default_csi_fast_path(),
            ascii_scan: default_ascii_scan(),
            simd_utf8: default_simd_utf8(),
            csi_params: [CsiParam::Integer(0); CSI_FAST_MAX_PARAMS],
            sgr_run: core::array::from_fn(|_| Sgr::Reset),
            last_sgr: LastSgr::new(),
        }
    }

    fn finish_recovery_stream_advance(&mut self, prior: Option<u64>, consumed: usize) {
        let Ok(consumed) = u64::try_from(consumed) else {
            self.recovery_stream_bytes = None;
            return;
        };
        self.recovery_stream_bytes = prior.and_then(|position| position.checked_add(consumed));
    }

    /// Consume the first retained control-string resource failure not yet
    /// observed by the caller. Replay and other fail-closed consumers must call
    /// this after every parse operation before applying the emitted action
    /// batch.
    pub fn take_string_sequence_error(&mut self) -> Option<crate::StringSequenceError> {
        self.state_machine
            .take_string_sequence_error()
            .or_else(|| self.state.borrow_mut().pending_string_sequence_error.take())
    }

    /// Returns whether this parser can be replaced by a fresh parser at the
    /// current raw-output boundary without losing incremental parse state.
    ///
    /// This is deliberately stricter than the underlying VT machine's Ground
    /// state. The semantic parser can also retain a Sixel, short-DCS, termcap,
    /// or tmux-control parser outside the VT transition table. Durable
    /// checkpoints must reject all of those states as well. Callers must bind
    /// a positive result to the exact processed raw-output sequence; using an
    /// earlier sequence with a newer terminal model would duplicate output on
    /// replay. This boolean is diagnostic state only; durable callers must use
    /// [`Self::recovery_ground_boundary`] so the parser remains borrowed and
    /// the exact byte watermark is carried into the transaction.
    #[must_use]
    pub fn is_recovery_ground(&self) -> bool {
        if !self.pending_actions.is_empty() || !self.state_machine.is_ground() {
            return false;
        }

        let state = self.state.borrow();
        state.sixel.is_none()
            && state.dcs.is_none()
            && !state.discarding_short_dcs
            && state.get_tcap.is_none()
            && {
                #[cfg(feature = "tmux_cc")]
                {
                    state.tmux_state.is_none()
                }
                #[cfg(not(feature = "tmux_cc"))]
                {
                    true
                }
            }
    }

    /// Mint a typed witness for this parser's exact raw-stream position only
    /// when no incremental parse state would be lost by replacement.
    ///
    /// Returning `None` on position overflow is intentional: a caller cannot
    /// bind a journal receipt to an ambiguous parser watermark.
    #[must_use]
    pub fn recovery_ground_boundary(&self) -> Option<RecoveryGroundBoundary<'_>> {
        let stream_bytes = self.recovery_stream_bytes?;
        self.is_recovery_ground().then_some(RecoveryGroundBoundary {
            _parser: self,
            stream_bytes,
        })
    }

    /// Enable or disable ground-state printable-run coalescing (Round-5 D1,
    /// ft-round5-gauntlet-lw0s7.10). Newly constructed parsers use the promoted
    /// default-on policy from [`default_print_batching`]; this setter is the
    /// explicit per-parser override used by A/B and equivalence proofs.
    ///
    /// When enabled, contiguous printable codepoints parsed in the VT *Ground*
    /// state are emitted as one `Action::PrintString` rather than a `Print`
    /// per codepoint. The action stream is otherwise byte-identical: every
    /// non-print action, and the rendered terminal state, is unchanged (a
    /// `PrintString("ab")` and a `Print('a'), Print('b')` pair drive the
    /// terminal performer identically — both accumulate into its print buffer).
    ///
    /// Exposed `#[doc(hidden)]` for the A/B bench harness and the equivalence
    /// test only; this is not part of the stable parser API.
    #[doc(hidden)]
    pub fn set_print_batching(&mut self, on: bool) {
        self.print_batching = on;
    }

    /// Returns whether ground-state printable-run coalescing is enabled.
    /// See [`Parser::set_print_batching`].
    #[doc(hidden)]
    pub fn print_batching(&self) -> bool {
        self.print_batching
    }

    /// Enable or disable table-driven CSI/OSC fast dispatch (Round-5 D2,
    /// ft-round5-gauntlet-lw0s7.12). Default is off unless the
    /// `parser-table-dispatch` feature is enabled.
    ///
    /// When enabled, CSI and OSC sequences are first offered to the table-driven
    /// fast decoders ([`CSI::parse_fast`] / `OperatingSystemCommand::parse_fast`),
    /// which produce a byte-identical action stream for the common shapes and
    /// fall back to the generic parser otherwise.
    ///
    /// Exposed `#[doc(hidden)]` for the A/B bench harness and the equivalence
    /// test only; this is not part of the stable parser API.
    #[doc(hidden)]
    pub fn set_table_dispatch(&mut self, on: bool) {
        self.state.borrow_mut().table_dispatch = on;
    }

    /// Returns whether table-driven CSI/OSC fast dispatch is enabled.
    /// See [`Parser::set_table_dispatch`].
    #[doc(hidden)]
    pub fn table_dispatch(&self) -> bool {
        self.state.borrow().table_dispatch
    }

    /// Turns the CSI fast path (ft-yccm0.3.2.4) on or off. It is on unless
    /// `FT_CSI_FAST_PATH` is falsey, and it runs only with print batching.
    ///
    /// At ground state it scans a complete CSI sequence straight from the
    /// bytes into a fixed parameter array, instead of feeding the state
    /// machine a byte at a time. The common finals are then decoded by a
    /// direct match, and SGR settings by an allocation-free decoder with a
    /// one-entry last-SGR cache. The settings of consecutive SGR sequences
    /// go to [`Handler::sgr`] together, and a lone codepoint between
    /// escapes is decoded in place rather than by the state machine's UTF-8
    /// decoder.
    ///
    /// Every other sequence is handed to the state machine or to
    /// [`CSI::parse`] exactly as before. The action stream is identical
    /// either way; off, every CSI goes through the state machine.
    pub fn set_csi_fast_path(&mut self, on: bool) {
        self.csi_fast_path = on;
    }

    /// Whether the CSI fast path is on. See [`Parser::set_csi_fast_path`].
    pub fn csi_fast_path(&self) -> bool {
        self.csi_fast_path
    }

    /// Chooses how the ground state finds the end of a printable-ASCII
    /// stretch (ft-yccm0.3.2.2). The default is [`AsciiScan::DEFAULT`]
    /// unless `FT_PARSER_SIMD` says otherwise; `FT_PARSER_SIMD=0` selects
    /// the scalar scan. It applies with print batching. Every choice yields
    /// the same action stream.
    pub fn set_ascii_scan(&mut self, scan: AsciiScan) {
        self.ascii_scan = scan;
    }

    /// The ground-state ASCII scan in use. See [`Parser::set_ascii_scan`].
    pub fn ascii_scan(&self) -> AsciiScan {
        self.ascii_scan
    }

    /// Chooses how the ground state checks the UTF-8 of a printable run
    /// (ft-yccm0.3.2.3). It is on unless `FT_PARSER_SIMD_UTF8` is falsey,
    /// and it applies with print batching.
    ///
    /// On:
    /// - the run's extent (up to the first control, DEL or C1 control) is
    ///   found with the [`AsciiScan`] in use;
    /// - its UTF-8 is validated once, by the same `core::str::from_utf8`
    ///   that hands the handler its `&str`;
    /// - a lone character with no printable byte after it is decoded and
    ///   checked in place.
    ///
    /// Off, every character is checked on its own and the run is then
    /// validated again: the oracle. The action stream is identical either
    /// way.
    pub fn set_simd_utf8(&mut self, on: bool) {
        self.simd_utf8 = on;
    }

    /// Whether printable runs are validated in one pass. See
    /// [`Parser::set_simd_utf8`].
    pub fn simd_utf8(&self) -> bool {
        self.simd_utf8
    }

    /// advance with tmux parser, bypass VTParse
    #[cfg(feature = "tmux_cc")]
    fn advance_tmux_bytes(&mut self, bytes: &[u8]) -> crate::Result<Vec<Event>> {
        let (result, sequence_error) = {
            let parser_state = self.state.borrow();
            let tmux_state = parser_state.tmux_state.as_ref().unwrap();
            let mut tmux_parser = tmux_state.borrow_mut();
            let result = tmux_parser.advance_bytes(bytes);
            let sequence_error = tmux_parser.take_sequence_error();
            (result, sequence_error)
        };
        if let Some(error) = sequence_error {
            let mut state = self.state.borrow_mut();
            if state.pending_string_sequence_error.is_none() {
                state.pending_string_sequence_error = Some(error);
            }
        }
        result
    }

    /// Decodes `bytes` and hands every action to `callback` as an [`Action`].
    pub fn parse<F: FnMut(Action)>(&mut self, bytes: &[u8], callback: F) {
        self.parse_with(bytes, &mut ActionCollector(callback));
    }

    /// Decodes `bytes` into `handler`, one call per action, with no
    /// intermediate `Action` storage; see [`Handler`]. Stream-position
    /// tracking is the same as [`Parser::parse`].
    pub fn parse_with<H: Handler + ?Sized>(&mut self, bytes: &[u8], handler: &mut H) {
        // Taking the prior position before invoking caller code makes unwind
        // fail closed. If a callback panics after the parser consumed any part
        // of this slice, the position remains `None` and this parser can never
        // mint an ambiguously old recovery watermark after the panic is caught.
        let prior_stream_bytes = self.recovery_stream_bytes.take();
        self.pending_sequence_bytes = 0;
        for action in core::mem::take(&mut self.pending_actions) {
            handler.action(action);
        }
        #[cfg(feature = "tmux_cc")]
        let is_tmux_mode: bool = self.state.borrow().tmux_state.is_some();
        #[cfg(feature = "tmux_cc")]
        if is_tmux_mode {
            match self.advance_tmux_bytes(bytes) {
                Ok(tmux_events) => {
                    handler.action(Action::DeviceControl(DeviceControlMode::TmuxEvents(
                        Box::new(tmux_events),
                    )));
                }
                Err(err_buf) => {
                    // capture bytes cannot be parsed
                    let unparsed_str = err_buf.to_string().to_owned();
                    let mut parser_state = self.state.borrow_mut();
                    parser_state.tmux_state = None;
                    let mut perform = Performer {
                        handler: &mut *handler,
                        state: &mut parser_state,
                    };
                    self.state_machine
                        .parse(unparsed_str.as_bytes(), &mut perform);
                }
            }
            self.finish_recovery_stream_advance(prior_stream_bytes, bytes.len());
            return;
        }

        if self.print_batching {
            self.parse_ground_batched(bytes, handler);
            self.finish_recovery_stream_advance(prior_stream_bytes, bytes.len());
            return;
        }

        {
            let mut perform = Performer {
                handler,
                state: &mut self.state.borrow_mut(),
            };
            self.state_machine.parse(bytes, &mut perform);
        }
        self.finish_recovery_stream_advance(prior_stream_bytes, bytes.len());
    }

    /// Streaming fast path for [`Parser::parse`] that coalesces maximal
    /// ground-state printable runs into one `Action::PrintString`.
    ///
    /// Correctness rests on a single invariant: a run scanned by
    /// [`scan_printable_run`] consists only of codepoints that, fed to the VT
    /// state machine while it is in the Ground state, emit exactly one
    /// `Action::Print` *and leave the machine in Ground*. The machine is
    /// therefore in Ground both before and after the run, so skipping the
    /// state machine for those bytes cannot desynchronise its internal state
    /// (`utf8_parser` is in its reset state whenever `is_ground()` holds).
    /// Every non-batchable byte — controls, ESC/CSI/OSC/DCS sequences,
    /// incomplete or invalid UTF-8, and C1 controls encoded as UTF-8 — is fed
    /// to the real state machine via `parse_byte`, so the emitted action stream
    /// is identical to the scalar path modulo print coalescing, including
    /// across chunk boundaries (an incomplete trailing multibyte sequence is
    /// deferred to the scalar path, which correctly parks in `Utf8Sequence`).
    ///
    /// With the CSI fast path on (ft-yccm0.3.2.4), two more ground-state
    /// shapes skip the state machine, by the same invariant. A run of one
    /// codepoint prints as the `Action::Print` the machine would emit. A
    /// complete CSI sequence that [`Parser::ground_csi`] takes dispatches as
    /// the machine would dispatch it, ending in Ground.
    fn parse_ground_batched<H: Handler + ?Sized>(&mut self, bytes: &[u8], handler: &mut H) {
        let csi_fast_path = self.csi_fast_path;
        let ascii_scan = self.ascii_scan;
        let simd_utf8 = self.simd_utf8;
        let mut extents = RunExtents::default();
        let n = bytes.len();
        let mut i = 0;
        while i < n {
            if self.state_machine.is_ground() {
                let byte = bytes[i];
                if can_start_printable_run(byte) {
                    match ground_run(bytes, i, ascii_scan, simd_utf8, &mut extents) {
                        GroundRun::Text { text, ascii } => {
                            if ascii {
                                handler.print_ascii_run(text);
                            } else {
                                handler.print_str(text);
                            }
                            i += text.len();
                            continue;
                        }
                        // A lone codepoint, such as the character between two
                        // SGR sequences, decoded here rather than byte by byte
                        // in the state machine's UTF-8 decoder.
                        GroundRun::One(c, len) if csi_fast_path => {
                            handler.print(c);
                            i += len;
                            continue;
                        }
                        _ => {}
                    }
                } else if byte == 0x1b && csi_fast_path {
                    let consumed = self.ground_csi(bytes, i, &mut *handler);
                    if consumed > 0 {
                        i += consumed;
                        continue;
                    }
                }
            }

            // Scalar segment: the byte at `i`, which no fast path took, goes
            // through the real state machine, and so does every byte after it
            // up to a ground-state boundary where a fast path may apply.
            let mut state = self.state.borrow_mut();
            let mut perform = Performer {
                handler: &mut *handler,
                state: &mut state,
            };
            loop {
                self.state_machine.parse_byte(bytes[i], &mut perform);
                i += 1;
                if i >= n {
                    break;
                }
                if self.state_machine.is_ground() {
                    let byte = bytes[i];
                    let boundary = if csi_fast_path {
                        byte == 0x1b || can_start_printable_run(byte)
                    } else {
                        can_start_printable_run(byte)
                            && matches!(
                                ground_run(bytes, i, ascii_scan, simd_utf8, &mut extents),
                                GroundRun::Text { .. }
                            )
                    };
                    if boundary {
                        break;
                    }
                }
            }
        }
    }

    /// The CSI fast path (ft-yccm0.3.2.4) at the ground-state ESC
    /// `bytes[start]`. Returns how many bytes it consumed, or 0 when the
    /// state machine must take the ESC.
    ///
    /// It takes the complete CSI sequences that [`scan_csi`] accepts, plus
    /// any further ones that follow immediately while each is an SGR:
    /// - An SGR whose parameter bytes are the last-SGR cache's is answered
    ///   from the cache.
    /// - Any other SGR is decoded by `decode_sgr`.
    /// - A common final is decoded by `decode_common_csi`.
    /// - Everything else goes to [`dispatch_csi`], exactly as the state
    ///   machine's `csi_dispatch` would send it.
    ///
    /// The settings of consecutive SGR sequences reach [`Handler::sgr`] in
    /// one call, with no flush or print boundary between them. They are
    /// handed over before any other action, so the stream order holds.
    fn ground_csi<H: Handler + ?Sized>(
        &mut self,
        bytes: &[u8],
        start: usize,
        handler: &mut H,
    ) -> usize {
        let mut i = start;
        let mut run = 0;
        while bytes.get(i) == Some(&0x1b) && bytes.get(i + 1) == Some(&b'[') {
            let body = i + 2;
            if let Some(len) = self.last_sgr.hit(&bytes[body..]) {
                let cached = &self.last_sgr.sgrs[..self.last_sgr.count];
                if run + cached.len() > SGR_RUN_MAX {
                    handler.sgr(&self.sgr_run[..run]);
                    run = 0;
                }
                self.sgr_run[run..run + cached.len()].clone_from_slice(cached);
                run += cached.len();
                i = body + len;
                continue;
            }
            let Some((fin, count)) = scan_csi(bytes, i, &mut self.csi_params) else {
                break;
            };
            let params = &self.csi_params[..count];
            let control = bytes[fin];
            if control == b'm' {
                if run + count / 2 + 1 > SGR_RUN_MAX {
                    handler.sgr(&self.sgr_run[..run]);
                    run = 0;
                }
                if let Some(decoded) = decode_sgr(params, &mut self.sgr_run[run..]) {
                    self.last_sgr
                        .store(&bytes[body..fin], &self.sgr_run[run..run + decoded]);
                    run += decoded;
                    i = fin + 1;
                    continue;
                }
            }
            // Any other sequence ends the run, which applies first.
            if run > 0 {
                handler.sgr(&self.sgr_run[..run]);
                run = 0;
            }
            let decoded =
                control != b'm' && decode_common_csi(params, control, &mut |csi| handler.csi(csi));
            if !decoded {
                let table_dispatch = self.state.borrow().table_dispatch;
                dispatch_csi(params, false, control, table_dispatch, &mut *handler);
            }
            i = fin + 1;
            break;
        }
        if run > 0 {
            handler.sgr(&self.sgr_run[..run]);
        }
        i - start
    }

    /// A specialized version of the parser that halts after recognizing the
    /// first action from the stream of bytes.  The return value is the action
    /// that was recognized and the length of the byte stream that was fed in
    /// to the parser to yield it.
    /// An action retained by `parse_first_as_vec` is returned with zero new
    /// bytes consumed. OSC dispatch occurs at ESC, before an ST's backslash.
    pub fn parse_first(&mut self, bytes: &[u8]) -> Option<(Action, usize)> {
        self.pending_sequence_bytes = 0;
        if let Some(action) = self.pending_actions.pop_front() {
            return Some((action, 0));
        }
        let prior_stream_bytes = self.recovery_stream_bytes.take();
        // holds the first action.  We need to use RefCell to deal with
        // the Performer holding a reference to this via the closure we set up.
        let first = RefCell::new(None);
        // will hold the iterator index when we emit an action
        let mut first_idx = None;
        {
            let mut perform = Performer {
                handler: &mut ActionCollector(|action| {
                    // capture the action, but only if it is the first one
                    // we've seen.  Preserve an existing one if any.
                    if first.borrow().is_some() {
                        return;
                    }
                    *first.borrow_mut() = Some(action);
                }),
                state: &mut self.state.borrow_mut(),
            };
            for (idx, b) in bytes.iter().enumerate() {
                self.state_machine.parse_byte(*b, &mut perform);
                if first.borrow().is_some() {
                    // if we recognized an action, record the iterator index
                    first_idx = Some(idx);
                    break;
                }
            }
        }

        let result = match (first.into_inner(), first_idx) {
            // if we matched an action, transform the iterator index to
            // the length of the string that was consumed (+1)
            (Some(action), Some(idx)) => Some((action, idx + 1)),
            _ => None,
        };
        let consumed = result
            .as_ref()
            .map_or(bytes.len(), |(_, consumed)| *consumed);
        self.finish_recovery_stream_advance(prior_stream_bytes, consumed);
        result
    }

    pub fn parse_as_vec(&mut self, bytes: &[u8]) -> Vec<Action> {
        let mut result = Vec::new();
        self.parse(bytes, |action| result.push(action));
        result
    }

    /// Similar to `parse_first` but collects all actions from the first sequence,
    /// and guarantees the state machine is in the ground state at the end of this
    /// sequence.
    /// Actions emitted before that boundary are retained across chunk calls
    /// and delivered exactly once, including when switching to another API.
    /// Exceeding either `MAX_FIRST_SEQUENCE_*` bound returns the emitted
    /// partial batch and this call's consumed byte count as an explicit error.
    pub fn parse_first_as_vec(
        &mut self,
        bytes: &[u8],
    ) -> Result<Option<(Vec<Action>, usize)>, FirstSequenceLimitExceeded> {
        let prior_stream_bytes = self.recovery_stream_bytes.take();
        let mut actions = core::mem::take(&mut self.pending_actions);
        let mut first_idx = None;
        for (idx, b) in bytes.iter().enumerate() {
            if actions.len() >= MAX_FIRST_SEQUENCE_ACTIONS
                || self.pending_sequence_bytes >= MAX_FIRST_SEQUENCE_BYTES
            {
                self.pending_sequence_bytes = 0;
                self.finish_recovery_stream_advance(prior_stream_bytes, idx);
                return Err(FirstSequenceLimitExceeded {
                    actions: actions.into_iter().collect(),
                    consumed: idx,
                });
            }
            self.pending_sequence_bytes += 1;
            self.state_machine.parse_byte(
                *b,
                &mut Performer {
                    handler: &mut ActionCollector(|action| actions.push_back(action)),
                    state: &mut self.state.borrow_mut(),
                },
            );
            if actions.len() > MAX_FIRST_SEQUENCE_ACTIONS {
                self.pending_sequence_bytes = 0;
                self.finish_recovery_stream_advance(prior_stream_bytes, idx + 1);
                return Err(FirstSequenceLimitExceeded {
                    actions: actions.into_iter().collect(),
                    consumed: idx + 1,
                });
            }
            if !actions.is_empty() && self.state_machine.is_ground() {
                // if we recognized any actions, record the iterator index
                first_idx = Some(idx);
                break;
            }
        }
        let result = if let Some(idx) = first_idx {
            self.pending_sequence_bytes = 0;
            Some((actions.into_iter().collect(), idx + 1))
        } else {
            self.pending_actions = actions;
            None
        };
        let consumed = result
            .as_ref()
            .map_or(bytes.len(), |(_, consumed)| *consumed);
        self.finish_recovery_stream_advance(prior_stream_bytes, consumed);
        Ok(result)
    }
}

struct Performer<'a, H: Handler + ?Sized + 'a> {
    handler: &'a mut H,
    state: &'a mut ParseState,
}

fn is_short_dcs(intermediates: &[u8], byte: u8) -> bool {
    if intermediates == &[b'$'] && byte == b'q' {
        // DECRQSS
        true
    } else {
        false
    }
}

/// Scan the maximal run of *ground-state-printable* bytes starting at `start`.
///
/// A codepoint belongs to the run iff feeding its bytes to the VT state machine
/// while it is in the Ground state would emit exactly one `Action::Print` and
/// leave the machine in Ground. The ground-state transition table
/// (`vtparse::transitions::ground`) prints `0x20..=0x7f` directly and routes
/// UTF-8 leads (`0xc2..=0xf4`) through `Utf8Sequence`, which prints the decoded
/// codepoint *unless* the special C1 case fires for a decoded value
/// `0x80..=0x9f` (see `VTParser::next_utf8`). The only multibyte sequences that
/// decode into that range are `0xC2 0x80..=0x9F`; every other complete, valid
/// sequence prints. (Multibyte encodings can never decode below `0x80`, so
/// controls and ASCII are unaffected.)
///
/// The run STOPS — leaving the byte(s) to the scalar `parse_byte` path — at the
/// first of:
///   * a C0 control or `ESC` (`0x00..=0x1f`),
///   * `DEL` (`0x7f`) — this fork prints it, but it is excluded for clarity;
///     deferring it to the scalar path is still byte-identical,
///   * a raw C1 byte / invalid UTF-8 lead / stray continuation
///     (`0x80..=0xc1`, `0xf5..=0xff`),
///   * an incomplete multibyte sequence at the end of `bytes` (preserves
///     cross-chunk streaming: the scalar path parks the machine in
///     `Utf8Sequence` to await the next chunk),
///   * an invalid multibyte sequence (rejected by `core::str::from_utf8`,
///     covering overlong forms and surrogates), and
///   * the C1-via-UTF-8 sequence `0xC2 0x80..=0x9F`.
///
/// Returns `(end_index, char_count)`, where `bytes[start..end_index]` is
/// guaranteed valid UTF-8 and `char_count` is the number of codepoints in it.
#[inline]
fn can_start_printable_run(byte: u8) -> bool {
    matches!(byte, 0x20..=0x7e) || byte >= 0xc2
}

/// The run described above. `ascii_scan` finds each ASCII stretch
/// (ft-yccm0.3.2.2); the result does not depend on which scan it is.
fn scan_printable_run(bytes: &[u8], start: usize, ascii_scan: AsciiScan) -> (usize, usize) {
    let n = bytes.len();
    let mut i = start;
    let mut chars = 0usize;
    while i < n {
        let b = bytes[i];
        if ascii::is_printable_ascii(b) {
            let stretch = ascii_stretch(bytes, i, ascii_scan);
            i += stretch;
            chars += stretch;
            continue;
        }
        let seq_len = match b {
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            // C0/C1/DEL/invalid lead/stray continuation: stop the run.
            _ => break,
        };
        if i + seq_len > n {
            // Incomplete multibyte sequence at the buffer boundary: defer to the
            // scalar path so the state machine parks in `Utf8Sequence`.
            break;
        }
        // C1-via-UTF-8 guard: `0xC2 0x80..=0x9F` decodes to U+0080..U+009F,
        // which vtparse executes as a C1 control rather than printing.
        if b == 0xc2 && matches!(bytes[i + 1], 0x80..=0x9f) {
            break;
        }
        match core::str::from_utf8(&bytes[i..i + seq_len]) {
            Ok(_) => {
                i += seq_len;
                chars += 1;
            }
            // Invalid encoding (bad continuation, overlong, surrogate): defer to
            // the scalar path, which emits the replacement char per vtparse.
            Err(_) => break,
        }
    }
    (i, chars)
}

/// The printable-ASCII stretch at `bytes[at]`, 0 when that byte is not
/// printable ASCII (ft-yccm0.3.2.2). A one-byte stretch (the character
/// between two escapes) is settled by the next byte, so it never reaches a
/// `std::simd` scan.
#[inline]
fn ascii_stretch(bytes: &[u8], at: usize, scan: AsciiScan) -> usize {
    if !bytes
        .get(at)
        .is_some_and(|&byte| ascii::is_printable_ascii(byte))
    {
        return 0;
    }
    match bytes.get(at + 1) {
        Some(&next) if ascii::is_printable_ascii(next) => scan.printable_len(&bytes[at..]),
        _ => 1,
    }
}

/// What starts at a ground-state byte that can start a printable run.
#[derive(Clone, Debug, PartialEq)]
enum GroundRun<'a> {
    /// No printable character: the state machine takes the byte.
    None,
    /// Exactly one character, and its length in bytes. It is printed as
    /// `Action::Print`, so no `String` is built for it.
    One(char, usize),
    /// Two or more characters, and whether they are all ASCII.
    Text { text: &'a str, ascii: bool },
}

/// Where the next control (or DEL) and the next `0xc2 0x00..=0x9f` pair lie
/// in one parse call's input, as absolute positions (ft-yccm0.3.2.3).
///
/// Runs are scanned left to right, so a position found from an earlier byte
/// still holds for every later byte up to it. Without the cache, each run
/// cut short by malformed UTF-8 would scan the rest of the input again,
/// which is quadratic on long input with no controls in it.
#[derive(Default)]
struct RunExtents {
    control: Option<usize>,
    c1: Option<usize>,
}

impl RunExtents {
    /// The first control or DEL at or after `from`, or the input's length.
    #[inline]
    fn control_from(&mut self, bytes: &[u8], from: usize, scan: AsciiScan) -> usize {
        match self.control {
            Some(at) if at >= from => at,
            _ => {
                let at = from + scan.control_free_len(&bytes[from..]);
                self.control = Some(at);
                at
            }
        }
    }

    /// The first `0xc2` at or after `from` followed by a byte below 0xa0,
    /// or the input's length. In valid UTF-8 that pair is a C1 control. In
    /// malformed UTF-8 the `0xc2` cannot start a character, so a run stops
    /// there either way.
    #[inline]
    fn c1_from(&mut self, bytes: &[u8], from: usize, scan: AsciiScan) -> usize {
        match self.c1 {
            Some(at) if at >= from => at,
            _ => {
                let at = scan
                    .c1_position(&bytes[from..])
                    .map_or(bytes.len(), |at| from + at);
                self.c1 = Some(at);
                at
            }
        }
    }
}

/// The maximal printable run at `bytes[start]`, whose bytes the state
/// machine would print one character at a time and end in Ground (see
/// [`scan_printable_run`]). With `simd_utf8` it is found by
/// [`ground_run_bulk`], otherwise by the oracle; both give the same run.
#[inline]
fn ground_run<'a>(
    bytes: &'a [u8],
    start: usize,
    scan: AsciiScan,
    simd_utf8: bool,
    extents: &mut RunExtents,
) -> GroundRun<'a> {
    if simd_utf8 {
        ground_run_bulk(bytes, start, scan, extents)
    } else {
        ground_run_scalar(bytes, start, scan)
    }
}

/// The oracle: [`scan_printable_run`] checks each character on its own,
/// and the run is validated again to hand it over as `&str`.
fn ground_run_scalar(bytes: &[u8], start: usize, scan: AsciiScan) -> GroundRun<'_> {
    let (end, chars) = scan_printable_run(bytes, start, scan);
    let run = &bytes[start..end];
    match chars {
        0 => GroundRun::None,
        1 => decode_scanned_char(run).map_or(GroundRun::None, |c| GroundRun::One(c, run.len())),
        // `scan_printable_run` only extends across complete, valid UTF-8.
        _ => core::str::from_utf8(run).map_or(GroundRun::None, |text| GroundRun::Text {
            text,
            ascii: run.len() == chars,
        }),
    }
}

/// The run with one UTF-8 validation (ft-yccm0.3.2.3).
///
/// Steps:
/// 1. The printable-ASCII stretch is scanned first. When a control, DEL or
///    the end of the input follows it, the run is all ASCII.
/// 2. Otherwise a lone character with no printable byte after it (T0's
///    emoji between two escapes) is decoded and checked in place by
///    [`decode_utf8_char`].
/// 3. Otherwise the run can reach as far as the first control, DEL or
///    `0xc2 0x00..=0x9f` pair (a C1 control in UTF-8, which the state
///    machine executes), found with the `std::simd` scans in
///    [`RunExtents`]. That stretch is validated once by
///    `core::str::from_utf8`, which also yields the `&str` handed over. The
///    run ends where the valid UTF-8 ends, at an invalid or incomplete
///    sequence, which the state machine then decodes, replacement
///    characters and all.
///
/// Only a run cut short by such a sequence is validated a second time, to
/// obtain its shorter `&str`.
///
/// A separate `std::simd` UTF-8 validator would not save that pass: safe
/// Rust only makes a `&str` from bytes by `core::str::from_utf8`, so it
/// would be a second validation, not a replacement.
fn ground_run_bulk<'a>(
    bytes: &'a [u8],
    start: usize,
    scan: AsciiScan,
    extents: &mut RunExtents,
) -> GroundRun<'a> {
    let ascii_end = start + ascii_stretch(bytes, start, scan);
    if bytes.get(ascii_end).is_none_or(|&next| next < 0x80) {
        return ascii_run(&bytes[start..ascii_end]);
    }
    if ascii_end == start {
        let Some((c, len)) = decode_utf8_char(&bytes[start..]) else {
            return GroundRun::None;
        };
        if bytes
            .get(start + len)
            .is_none_or(|&next| next < 0x20 || next == 0x7f)
        {
            return GroundRun::One(c, len);
        }
    }

    let limit = extents
        .control_from(bytes, ascii_end, scan)
        .min(extents.c1_from(bytes, ascii_end, scan));
    let candidate = &bytes[start..limit];
    let text = match core::str::from_utf8(candidate) {
        Ok(text) => Some(text),
        Err(error) => core::str::from_utf8(&candidate[..error.valid_up_to()]).ok(),
    };
    let Some(text) = text else {
        return GroundRun::None;
    };
    let mut chars = text.chars();
    match (chars.next(), chars.next()) {
        (None, _) => GroundRun::None,
        (Some(c), None) => GroundRun::One(c, text.len()),
        _ => GroundRun::Text {
            text,
            ascii: text.len() <= ascii_end - start,
        },
    }
}

/// A run of printable ASCII.
#[inline]
fn ascii_run(run: &[u8]) -> GroundRun<'_> {
    match run {
        [] => GroundRun::None,
        [byte] => GroundRun::One(char::from(*byte), 1),
        _ => core::str::from_utf8(run).map_or(GroundRun::None, |text| GroundRun::Text {
            text,
            ascii: true,
        }),
    }
}

/// One complete, well-formed UTF-8 sequence of two to four bytes at the
/// start of `bytes` (Unicode Table 3-7) that is not a C1 control: its
/// character and length (ft-yccm0.3.2.3). Checked and decoded here, with no
/// call into `core::str::from_utf8`. `None` for anything else, including
/// ASCII, an incomplete sequence, overlongs, surrogates, values beyond
/// U+10FFFF, and `0xc2 0x80..=0x9f`.
#[inline]
fn decode_utf8_char(bytes: &[u8]) -> Option<(char, usize)> {
    let continuation = |at: usize| -> Option<u32> {
        bytes
            .get(at)
            .filter(|b| matches!(b, 0x80..=0xbf))
            .map(|&b| u32::from(b & 0x3f))
    };
    let lead = *bytes.first()?;
    let (value, len) = match lead {
        0xc2..=0xdf => {
            let low = continuation(1)?;
            // `0xc2 0x80..=0x9f` is a C1 control.
            if lead == 0xc2 && low < 0x20 {
                return None;
            }
            ((u32::from(lead & 0x1f) << 6) | low, 2)
        }
        0xe0..=0xef => {
            let second = *bytes.get(1)?;
            let allowed = match lead {
                0xe0 => 0xa0..=0xbf,
                0xed => 0x80..=0x9f,
                _ => 0x80..=0xbf,
            };
            if !allowed.contains(&second) {
                return None;
            }
            let value =
                (u32::from(lead & 0x0f) << 12) | (u32::from(second & 0x3f) << 6) | continuation(2)?;
            (value, 3)
        }
        0xf0..=0xf4 => {
            let second = *bytes.get(1)?;
            let allowed = match lead {
                0xf0 => 0x90..=0xbf,
                0xf4 => 0x80..=0x8f,
                _ => 0x80..=0xbf,
            };
            if !allowed.contains(&second) {
                return None;
            }
            let value = (u32::from(lead & 0x07) << 18)
                | (u32::from(second & 0x3f) << 12)
                | (continuation(2)? << 6)
                | continuation(3)?;
            (value, 4)
        }
        _ => return None,
    };
    Some((char::from_u32(value)?, len))
}

/// Decodes one codepoint [`scan_printable_run`] accepted: a printable ASCII
/// byte or one complete, valid UTF-8 sequence.
#[inline]
fn decode_scanned_char(sequence: &[u8]) -> Option<char> {
    let value = match *sequence {
        [a] => u32::from(a),
        [a, b] => (u32::from(a & 0x1f) << 6) | u32::from(b & 0x3f),
        [a, b, c] => (u32::from(a & 0x0f) << 12) | (u32::from(b & 0x3f) << 6) | u32::from(c & 0x3f),
        [a, b, c, d] => {
            (u32::from(a & 0x07) << 18)
                | (u32::from(b & 0x3f) << 12)
                | (u32::from(c & 0x3f) << 6)
                | u32::from(d & 0x3f)
        }
        _ => return None,
    };
    char::from_u32(value)
}

/// Scans the CSI sequence whose ESC is `bytes[esc]` for the CSI fast path
/// (ft-yccm0.3.2.4). Its parameters go into `params` exactly as the state
/// machine hands them to `csi_dispatch`:
/// - digits accumulate, saturating, into an `Integer`;
/// - each `;` or `:` is a `P` after the integer before it;
/// - a leading private marker (`<`, `=`, `>`, `?`) is a `P` first.
///
/// Returns the index of the final byte and the number of parameters.
///
/// It returns `None`, leaving the bytes to the state machine, for any
/// sequence it does not reproduce:
/// - no `[` after the ESC;
/// - a sequence still incomplete at the end of `bytes`;
/// - intermediates;
/// - a second or misplaced marker, or a leading `:` (the state machine
///   ignores those sequences);
/// - a control, DEL or non-ASCII byte inside;
/// - more parameters than `params` holds.
///
/// The state machine never truncates the parameters of a sequence this
/// accepts.
fn scan_csi(bytes: &[u8], esc: usize, params: &mut [CsiParam]) -> Option<(usize, usize)> {
    if bytes.get(esc + 1) != Some(&b'[') {
        return None;
    }
    let first = esc + 2;
    let mut i = first;
    let mut count = 0;
    if let marker @ 0x3c..=0x3f = *bytes.get(i)? {
        *params.get_mut(count)? = CsiParam::P(marker);
        count += 1;
        i += 1;
    }
    let mut current: Option<i64> = None;
    loop {
        match *bytes.get(i)? {
            digit @ b'0'..=b'9' => {
                let value = current.unwrap_or(0);
                current = Some(
                    value
                        .saturating_mul(10)
                        .saturating_add(i64::from(digit - b'0')),
                );
            }
            separator @ (b':' | b';') => {
                if separator == b':' && i == first {
                    return None;
                }
                if let Some(value) = current.take() {
                    *params.get_mut(count)? = CsiParam::Integer(value);
                    count += 1;
                }
                *params.get_mut(count)? = CsiParam::P(separator);
                count += 1;
            }
            0x40..=0x7e => {
                if let Some(value) = current {
                    *params.get_mut(count)? = CsiParam::Integer(value);
                    count += 1;
                }
                return Some((i, count));
            }
            _ => return None,
        }
        i += 1;
    }
}

/// Hands `handler` the items of a dispatched CSI sequence: the D2 table
/// decoder's when table dispatch is on and it takes the sequence, otherwise
/// [`CSI::parse`]'s. The state machine's `csi_dispatch` and the CSI fast
/// path's fallback share it.
fn dispatch_csi<H: Handler + ?Sized>(
    params: &[CsiParam],
    parameters_truncated: bool,
    control: u8,
    table_dispatch: bool,
    handler: &mut H,
) {
    // D2 (ft-round5-gauntlet-lw0s7.12): offer the sequence to the
    // table-driven fast decoder first; it emits a byte-identical action
    // stream for the common shapes (and emits nothing when it declines).
    if table_dispatch
        && CSI::parse_fast(
            params,
            parameters_truncated,
            control as char,
            &mut |action| handler.csi(action),
        )
    {
        return;
    }
    for action in CSI::parse(params, parameters_truncated, control as char) {
        handler.csi(action);
    }
}

impl<'a, H: Handler + ?Sized> VTActor for Performer<'a, H> {
    #[inline]
    fn print(&mut self, c: char) {
        self.handler.print(c);
    }

    fn execute_c0_or_c1(&mut self, byte: u8) {
        match FromPrimitive::from_u8(byte) {
            Some(code) => self.handler.control(code),
            None => error!(
                "impossible C0/C1 control code {:?} 0x{:x} was dropped",
                byte as char, byte
            ),
        }
    }

    fn apc_dispatch(&mut self, data: Vec<u8>) {
        if let Some(img) = super::KittyImage::parse_apc(&data) {
            self.handler.action(Action::KittyImage(Box::new(img)))
        } else {
            log::trace!("Ignoring APC data: {:?}", String::from_utf8_lossy(&data));
        }
    }

    fn dcs_hook(
        &mut self,
        byte: u8,
        params: &[i64],
        intermediates: &[u8],
        ignored_extra_intermediates: bool,
    ) {
        let max_string_sequence_bytes = self.state.max_string_sequence_bytes;
        self.state.sixel.take();
        self.state.get_tcap.take();
        self.state.dcs.take();
        self.state.discarding_short_dcs = false;
        if byte == b'q' && intermediates.is_empty() && !ignored_extra_intermediates {
            self.state
                .sixel
                .replace(SixelBuilder::new_with_max_retained_bytes(
                    params,
                    max_string_sequence_bytes,
                ));
        } else if byte == b'q' && intermediates == [b'+'] {
            self.state
                .get_tcap
                .replace(GetTcapBuilder::new(max_string_sequence_bytes));
        } else if !ignored_extra_intermediates && is_short_dcs(intermediates, byte) {
            self.state.dcs.replace(ShortDeviceControl {
                params: params.to_vec(),
                intermediates: intermediates.to_vec(),
                byte,
                data: vec![],
            });
        } else {
            #[cfg(feature = "tmux_cc")]
            if byte == b'p' && params == [1000] {
                // into tmux_cc mode
                self.state.borrow_mut().tmux_state = Some(RefCell::new(
                    crate::tmux_cc::Parser::new_with_max_retained_bytes(max_string_sequence_bytes),
                ));
            }
            self.handler
                .action(Action::DeviceControl(DeviceControlMode::Enter(Box::new(
                    EnterDeviceControlMode {
                        byte,
                        params: params.to_vec(),
                        intermediates: intermediates.to_vec(),
                        ignored_extra_intermediates,
                    },
                ))));
        }
    }

    fn dcs_put(&mut self, data: u8) {
        if self.state.discarding_short_dcs {
            return;
        }
        if self.state.dcs.is_some() {
            let mut sequence_error = None;
            let maximum = self
                .state
                .max_string_sequence_bytes
                .min(MAX_SHORT_DCS_BYTES);
            if let Some(dcs) = self.state.dcs.as_mut() {
                if dcs.data.len() >= maximum {
                    sequence_error = Some(crate::StringSequenceError::LimitExceeded {
                        kind: crate::StringSequenceKind::ShortDeviceControl,
                        maximum,
                    });
                } else if dcs.data.try_reserve(1).is_err() {
                    sequence_error = Some(crate::StringSequenceError::AllocationFailed {
                        kind: crate::StringSequenceKind::ShortDeviceControl,
                    });
                } else {
                    dcs.data.push(data);
                }
            }

            if let Some(error) = sequence_error {
                self.state.discarding_short_dcs = true;
                self.state.dcs.take();
                if self.state.pending_string_sequence_error.is_none() {
                    self.state.pending_string_sequence_error = Some(error);
                }
                log::warn!(
                    "short DCS payload violated its bounded retention policy; discarding the command until DCS terminator"
                );
            }
        } else if let Some(sixel) = self.state.sixel.as_mut() {
            sixel.push(data);
            if let Some(error) = sixel.take_sequence_error() {
                if self.state.pending_string_sequence_error.is_none() {
                    self.state.pending_string_sequence_error = Some(error);
                }
            }
        } else if let Some(tcap) = self.state.get_tcap.as_mut() {
            tcap.push(data);
            if let Some(error) = tcap.take_sequence_error() {
                if self.state.pending_string_sequence_error.is_none() {
                    self.state.pending_string_sequence_error = Some(error);
                }
            }
        } else {
            #[cfg(feature = "tmux_cc")]
            if let Some(tmux_state) = &self.state.tmux_state {
                let (result, sequence_error) = {
                    let mut tmux_parser = tmux_state.borrow_mut();
                    let result = tmux_parser.advance_byte(data);
                    let sequence_error = tmux_parser.take_sequence_error();
                    (result, sequence_error)
                };
                if let Some(error) = sequence_error {
                    if self.state.pending_string_sequence_error.is_none() {
                        self.state.pending_string_sequence_error = Some(error);
                    }
                }
                match result {
                    Ok(optional_events) => {
                        if let Some(tmux_event) = optional_events {
                            self.handler.action(Action::DeviceControl(
                                DeviceControlMode::TmuxEvents(Box::new(vec![tmux_event])),
                            ));
                        }
                    }
                    Err(_) => {
                        self.state.tmux_state = None; // drop tmux state
                    }
                }
                return;
            }
            self.handler
                .action(Action::DeviceControl(DeviceControlMode::Data(data)));
        }
    }

    fn dcs_unhook(&mut self) {
        if core::mem::take(&mut self.state.discarding_short_dcs) {
            self.state.dcs.take();
            return;
        }
        if let Some(dcs) = self.state.dcs.take() {
            self.handler.action(Action::DeviceControl(
                DeviceControlMode::ShortDeviceControl(Box::new(dcs)),
            ));
        } else if let Some(mut sixel) = self.state.sixel.take() {
            sixel.finish();
            if let Some(error) = sixel.take_sequence_error() {
                if self.state.pending_string_sequence_error.is_none() {
                    self.state.pending_string_sequence_error = Some(error);
                }
            }
            if sixel.should_emit() {
                self.handler.action(Action::Sixel(Box::new(sixel.sixel)));
            }
        } else if let Some(tcap) = self.state.get_tcap.take() {
            let (names, error) = tcap.finish();
            if let Some(error) = error {
                if self.state.pending_string_sequence_error.is_none() {
                    self.state.pending_string_sequence_error = Some(error);
                }
            }
            if let Some(names) = names {
                self.handler.action(Action::XtGetTcap(names));
            }
        } else {
            self.handler
                .action(Action::DeviceControl(DeviceControlMode::Exit));
        }
    }

    fn osc_dispatch(&mut self, osc: &[&[u8]]) {
        // D2 (ft-round5-gauntlet-lw0s7.12): gated table-driven fast decoder.
        let parsed = if self.state.table_dispatch {
            OperatingSystemCommand::parse_fast(osc)
        } else {
            OperatingSystemCommand::parse(osc)
        };
        self.handler
            .action(Action::OperatingSystemCommand(Box::new(parsed)));
    }

    fn csi_dispatch(&mut self, params: &[CsiParam], parameters_truncated: bool, control: u8) {
        dispatch_csi(
            params,
            parameters_truncated,
            control,
            self.state.table_dispatch,
            &mut *self.handler,
        );
    }

    fn esc_dispatch(
        &mut self,
        _params: &[i64],
        intermediates: &[u8],
        ignored_extra_intermediates: bool,
        control: u8,
    ) {
        // Esc represents at most one intermediate. Dropping extra bytes would
        // turn an unsupported sequence into a different command: ESC SP SP X
        // would become SOS and swallow subsequent printable text on replay.
        // Overflow also invalidates the sequence even if the retained prefix
        // happens to fit the representation.
        if ignored_extra_intermediates || intermediates.len() > 1 {
            return;
        }
        self.handler.esc(Esc::parse(
            if intermediates.len() == 1 {
                Some(intermediates[0])
            } else {
                None
            },
            control,
        ));
    }
}

#[cfg(all(test, feature = "std"))]
mod test {
    use super::*;
    use crate::color::ColorSpec;
    use crate::csi::{
        CharacterPath, DecPrivateMode, DecPrivateModeCode, Device, Intensity, Mode, Sgr, Underline,
        Window, XtSmGraphics, XtSmGraphicsItem, XtermKeyModifierResource,
    };
    use crate::{EscCode, OneBased};
    use k9::assert_equal as assert_eq;
    use std::io::Write;

    fn encode(seq: &Vec<Action>) -> String {
        let mut res = Vec::new();
        for s in seq {
            write!(res, "{}", s).unwrap();
        }
        String::from_utf8(res).unwrap()
    }

    #[test]
    fn print_batching_default_policy_is_on_unless_either_gate_is_falsey() {
        assert!(print_batching_default_for_values(None, None));
        assert!(print_batching_default_for_values(Some("true"), Some("on")));
        assert!(print_batching_default_for_values(
            Some("future-value"),
            Some("1")
        ));

        for falsey in ["", "0", "false", "FALSE", " off ", "No"] {
            assert!(!print_batching_default_for_values(Some(falsey), None));
            assert!(!print_batching_default_for_values(None, Some(falsey)));
            assert!(!print_batching_default_for_values(
                Some("true"),
                Some(falsey),
            ));
        }
    }

    #[test]
    fn recovery_ground_requires_complete_streaming_sequences() {
        let mut parser = Parser::new();
        assert!(parser.is_recovery_ground());

        parser.parse(b"\x1b[38;2", |_| {});
        assert!(!parser.is_recovery_ground());
        parser.parse(b";1;2;3m", |_| {});
        assert!(parser.is_recovery_ground());

        parser.parse(b"\x1b]2;partial title", |_| {});
        assert!(!parser.is_recovery_ground());
        parser.parse(b"\x07", |_| {});
        assert!(parser.is_recovery_ground());

        parser.parse(b"\x1bP$qm", |_| {});
        assert!(!parser.is_recovery_ground());
        parser.parse(b"\x1b\\", |_| {});
        assert!(parser.is_recovery_ground());

        parser.parse(&[0xe2, 0x82], |_| {});
        assert!(!parser.is_recovery_ground());
        parser.parse(&[0xac], |_| {});
        assert!(parser.is_recovery_ground());
    }

    #[test]
    fn recovery_ground_boundary_tracks_the_exact_consumed_stream_watermark() {
        let mut parser = Parser::new();
        assert_eq!(
            parser
                .recovery_ground_boundary()
                .expect("fresh parser is ground")
                .stream_bytes(),
            0
        );

        parser.parse(b"prefix\x1b[38;2", |_| {});
        assert!(
            parser.recovery_ground_boundary().is_none(),
            "a typed boundary must not be minted for an incomplete CSI"
        );

        parser.parse(b";1;2;3m", |_| {});
        assert_eq!(
            parser
                .recovery_ground_boundary()
                .expect("completed CSI returns to ground")
                .stream_bytes(),
            u64::try_from(b"prefix\x1b[38;2;1;2;3m".len()).expect("fixture length fits")
        );
    }

    #[test]
    fn recovery_ground_boundary_counts_only_parse_first_consumption() {
        let mut parser = Parser::new();
        let (action, consumed) = parser
            .parse_first(b"a\x1b[partial")
            .expect("first printable action");
        assert_eq!(action, Action::Print('a'));
        assert_eq!(consumed, 1);
        assert_eq!(
            parser
                .recovery_ground_boundary()
                .expect("parser stopped at a ground boundary")
                .stream_bytes(),
            1
        );
    }

    #[test]
    fn recovery_ground_boundary_fails_closed_after_watermark_overflow() {
        let mut parser = Parser::new();
        parser.recovery_stream_bytes = Some(u64::MAX);
        parser.parse(b"x", |_| {});

        assert!(parser.is_recovery_ground());
        assert!(parser.recovery_ground_boundary().is_none());
    }

    #[test]
    fn recovery_ground_boundary_fails_closed_after_callback_unwind() {
        let mut parser = Parser::new();
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            parser.parse(b"printable", |_| panic!("injected callback panic"));
        }));

        assert!(unwind.is_err());
        assert!(parser.is_recovery_ground());
        assert!(
            parser.recovery_ground_boundary().is_none(),
            "a caught callback unwind must not leave the old stream watermark reusable"
        );

        parser.parse(b"later-ground-data", |_| {});
        assert!(
            parser.recovery_ground_boundary().is_none(),
            "later successful parsing must not resurrect an ambiguous pre-unwind watermark"
        );
    }

    #[test]
    fn recovery_ground_boundary_counts_parse_first_as_vec_consumption() {
        let mut parser = Parser::new();
        let (actions, consumed) = parser
            .parse_first_as_vec(b"a\x1b[partial")
            .expect("bounded sequence")
            .expect("first ground action batch");

        assert_eq!(actions, vec![Action::Print('a')]);
        assert_eq!(consumed, 1);
        assert_eq!(
            parser
                .recovery_ground_boundary()
                .expect("parser stopped at the first exact ground boundary")
                .stream_bytes(),
            1
        );
    }

    #[cfg(feature = "tmux_cc")]
    #[test]
    fn recovery_ground_rejects_active_tmux_control_parser() {
        let mut parser = Parser::new();
        parser.parse(b"\x1bP1000p\x1b\\", |_| {});

        assert!(parser.state_machine.is_ground());
        assert!(!parser.is_recovery_ground());
    }

    #[test]
    fn recovery_checkpoint_parser_identity_is_versioned_and_nonempty() {
        assert_eq!(
            RECOVERY_CHECKPOINT_PARSER_ID,
            "frankenterm.escape-parser.recovery-ground.v3"
        );
    }

    #[test]
    fn string_sequence_limit_error_is_exposed_for_fail_closed_callers() {
        let mut parser = Parser::new_with_max_string_sequence_bytes(4);
        let mut actions = Vec::new();

        parser.parse(b"\x1b]ab", |action| actions.push(action));
        parser.parse(b"cde\x07", |action| actions.push(action));

        assert!(actions.is_empty());
        assert_eq!(
            parser.take_string_sequence_error(),
            Some(crate::StringSequenceError::LimitExceeded {
                kind: crate::StringSequenceKind::OperatingSystemCommand,
                maximum: 4,
            })
        );
        assert_eq!(parser.take_string_sequence_error(), None);
        assert!(parser.is_recovery_ground());
    }

    #[test]
    fn sixel_retained_limit_error_is_exposed_for_fail_closed_callers() {
        let max_retained_bytes = core::mem::size_of::<crate::SixelData>() * 2;
        let mut parser = Parser::new_with_max_string_sequence_bytes(max_retained_bytes);
        let mut actions = Vec::new();

        parser.parse(b"\x1bPq", |action| actions.push(action));
        parser.parse(b"???\x9c", |action| actions.push(action));

        assert!(actions.is_empty());
        assert_eq!(
            parser.take_string_sequence_error(),
            Some(crate::StringSequenceError::LimitExceeded {
                kind: crate::StringSequenceKind::Sixel,
                maximum: max_retained_bytes,
            })
        );
        assert_eq!(parser.take_string_sequence_error(), None);
        assert!(parser.is_recovery_ground());
    }

    #[test]
    fn caller_string_cap_clamps_short_dcs_and_resets_at_terminator() {
        let mut parser = Parser::new_with_max_string_sequence_bytes(2);
        let actions = parser.parse_as_vec(b"\x1bP$qabc\x1b\\\x1bP$qOK\x1b\\");
        let short_dcs: Vec<_> = actions
            .into_iter()
            .filter_map(|action| match action {
                Action::DeviceControl(DeviceControlMode::ShortDeviceControl(dcs)) => Some(*dcs),
                _ => None,
            })
            .collect();

        assert_eq!(short_dcs.len(), 1);
        assert_eq!(short_dcs[0].data, b"OK".to_vec());
        assert_eq!(
            parser.take_string_sequence_error(),
            Some(crate::StringSequenceError::LimitExceeded {
                kind: crate::StringSequenceKind::ShortDeviceControl,
                maximum: 2,
            })
        );
    }

    #[test]
    fn caller_string_cap_clamps_xtgettcap_and_suppresses_partial_dispatch() {
        let mut parser = Parser::new_with_max_string_sequence_bytes(4);
        let actions = parser.parse_as_vec(b"\x1bP+q544e4\x1b\\\x1bP+q544e\x1b\\");
        let tcap_names: Vec<_> = actions
            .into_iter()
            .filter_map(|action| match action {
                Action::XtGetTcap(names) => Some(names),
                _ => None,
            })
            .collect();

        assert_eq!(tcap_names, vec![vec!["TN".to_string()]]);
        assert_eq!(
            parser.take_string_sequence_error(),
            Some(crate::StringSequenceError::LimitExceeded {
                kind: crate::StringSequenceKind::XtGetTcap,
                maximum: 4,
            })
        );
    }

    #[cfg(feature = "tmux_cc")]
    #[test]
    fn caller_string_cap_clamps_tmux_line_and_recovers_at_newline() {
        const CALLER_CAP: usize = 32;
        let mut parser = Parser::new_with_max_string_sequence_bytes(CALLER_CAP);
        let mut actions = Vec::new();
        parser.parse(b"\x1bP1000p", |action| actions.push(action));
        parser.parse(&[b'x'; CALLER_CAP + 1], |action| actions.push(action));
        parser.parse(b"\n%sessions-changed\n", |action| actions.push(action));

        assert!(actions.iter().any(|action| {
            matches!(
                action,
                Action::DeviceControl(DeviceControlMode::TmuxEvents(events))
                    if events.as_slice() == [Event::SessionsChanged]
            )
        }));
        assert_eq!(
            parser.take_string_sequence_error(),
            Some(crate::StringSequenceError::LimitExceeded {
                kind: crate::StringSequenceKind::TmuxControl,
                maximum: CALLER_CAP,
            })
        );
    }

    #[cfg(feature = "tmux_cc")]
    #[test]
    fn caller_string_cap_clamps_tmux_guarded_output() {
        const CALLER_CAP: usize = 64;
        let mut parser = Parser::new_with_max_string_sequence_bytes(CALLER_CAP);
        let mut actions = Vec::new();
        parser.parse(b"\x1bP1000p", |action| actions.push(action));
        let output_line = "x".repeat(40);
        let guarded = format!("%begin 1 2 3\n{output_line}\n{output_line}\n%end 1 2 3\n");
        parser.parse(guarded.as_bytes(), |action| actions.push(action));

        assert!(actions.iter().any(|action| {
            matches!(
                action,
                Action::DeviceControl(DeviceControlMode::TmuxEvents(events))
                    if matches!(events.as_slice(), [Event::Guarded(guarded)]
                        if guarded.error && guarded.output.len() <= CALLER_CAP)
            )
        }));
        assert_eq!(
            parser.take_string_sequence_error(),
            Some(crate::StringSequenceError::LimitExceeded {
                kind: crate::StringSequenceKind::TmuxControl,
                maximum: CALLER_CAP,
            })
        );
    }

    // <https://github.com/markbt/streampager/issues/57>
    #[test]
    fn osc_bel_parse_first_as_vec() {
        let data = b"\x1b]8;;http://example.com\x07example\x1b]8;;\x07";
        let mut p = Parser::new();

        let mut offset = 0;
        let mut actions = vec![];
        while let Some((mut act, off)) = p.parse_first_as_vec(&data[offset..]).unwrap() {
            actions.append(&mut act);
            offset += off;
        }

        k9::snapshot!(
            actions,
            r#"
[
    OperatingSystemCommand(
        SetHyperlink(
            Some(
                Hyperlink {
                    param_count: 0,
                    uri_bytes: 18,
                    implicit: false,
                    semantic_text: "[REDACTED]",
                },
            ),
        ),
    ),
    Print(
        'e',
    ),
    Print(
        'x',
    ),
    Print(
        'a',
    ),
    Print(
        'm',
    ),
    Print(
        'p',
    ),
    Print(
        'l',
    ),
    Print(
        'e',
    ),
    OperatingSystemCommand(
        SetHyperlink(
            None,
        ),
    ),
]
"#
        );
    }

    // <https://github.com/markbt/streampager/issues/57>
    #[test]
    fn osc_st_parse_first_as_vec() {
        // This string includes an assitional trailing ST sequence which should
        // be parsed separately.
        let data = b"\x1b]8;;http://example.com\x1b\\example\x1b]8;;\x1b\\\x1b\\";
        let mut p = Parser::new();

        let mut offset = 0;
        let mut actions = vec![];
        let mut slices = vec![];
        while let Some((act, off)) = p.parse_first_as_vec(&data[offset..]).unwrap() {
            // Store each vec of actions so we can confirm that the ST sequence is bundled with the
            // OSC SetHyperlink command.
            actions.push(act);
            // Additionally store all non-single-character slices so we can confirm these are split
            // correctly.
            if off > 1 {
                slices.push(&data[offset..offset + off]);
            }
            offset += off;
        }

        assert_eq!(
            slices,
            vec![
                b"\x1b]8;;http://example.com\x1b\\".as_slice(),
                b"\x1b]8;;\x1b\\".as_slice(),
                b"\x1b\\".as_slice()
            ]
        );

        k9::snapshot!(
            actions,
            r#"
[
    [
        OperatingSystemCommand(
            SetHyperlink(
                Some(
                    Hyperlink {
                        param_count: 0,
                        uri_bytes: 18,
                        implicit: false,
                        semantic_text: "[REDACTED]",
                    },
                ),
            ),
        ),
        Esc(
            Code(
                StringTerminator,
            ),
        ),
    ],
    [
        Print(
            'e',
        ),
    ],
    [
        Print(
            'x',
        ),
    ],
    [
        Print(
            'a',
        ),
    ],
    [
        Print(
            'm',
        ),
    ],
    [
        Print(
            'p',
        ),
    ],
    [
        Print(
            'l',
        ),
    ],
    [
        Print(
            'e',
        ),
    ],
    [
        OperatingSystemCommand(
            SetHyperlink(
                None,
            ),
        ),
        Esc(
            Code(
                StringTerminator,
            ),
        ),
    ],
    [
        Esc(
            Code(
                StringTerminator,
            ),
        ),
    ],
]
"#
        );
    }

    #[test]
    fn basic_parse() {
        let mut p = Parser::new();
        // This assertion is intentionally representation-specific.  Pin the
        // mode so the supported environment-level batching opt-out cannot
        // turn an otherwise-correct test run red.
        p.set_print_batching(true);
        let actions = p.parse_as_vec(b"hello");
        assert_eq!(vec![Action::PrintString("hello".to_string())], actions);
        assert_eq!(encode(&actions), "hello");
    }

    #[test]
    fn scan_printable_run_boundaries() {
        // Every ASCII scan must find the same run.
        fn scan_with_every_scan(bytes: &[u8], start: usize) -> (usize, usize) {
            let oracle = scan_printable_run(bytes, start, AsciiScan::Scalar);
            for scan in AsciiScan::ALL {
                assert_eq!(scan_printable_run(bytes, start, scan), oracle, "{:?}", scan);
            }
            oracle
        }
        // Pure ASCII printable run, stops at first control.
        assert_eq!(scan_with_every_scan(b"abc\n", 0), (3, 3));
        // DEL terminates the run.
        assert_eq!(scan_with_every_scan(b"ab\x7fc", 0), (2, 2));
        // ESC terminates the run.
        assert_eq!(scan_with_every_scan(b"ab\x1b[m", 0), (2, 2));
        // 2-byte UTF-8 (é) counts as one char.
        assert_eq!(scan_with_every_scan(b"a\xc3\xa9b", 0), (4, 3));
        // Latin-1 NBSP (U+00A0) prints in ground -> batchable.
        assert_eq!(scan_with_every_scan(b"a\xc2\xa0b", 0), (4, 3));
        // 3-byte (€) and 4-byte (🚀) sequences.
        assert_eq!(scan_with_every_scan("€".as_bytes(), 0), (3, 1));
        assert_eq!(scan_with_every_scan("🚀".as_bytes(), 0), (4, 1));
        // C1-via-UTF-8 (0xC2 0x9B == U+009B == CSI) stops the run immediately.
        assert_eq!(scan_with_every_scan(b"\xc2\x9bx", 0), (0, 0));
        assert_eq!(scan_with_every_scan(b"ab\xc2\x9bx", 0), (2, 2));
        // The 0x9F/0xA0 boundary: 0x9F is C1 (stop), 0xA0 prints.
        assert_eq!(scan_with_every_scan(b"\xc2\x9f", 0), (0, 0));
        assert_eq!(scan_with_every_scan(b"\xc2\xa0", 0), (2, 1));
        // Incomplete trailing multibyte sequence is deferred (not consumed).
        assert_eq!(scan_with_every_scan(b"ab\xe2\x82", 0), (2, 2));
        assert_eq!(scan_with_every_scan(b"ab\xc3", 0), (2, 2));
        // Invalid encodings stop the run.
        assert_eq!(scan_with_every_scan(b"a\xffb", 0), (1, 1));
        assert_eq!(scan_with_every_scan(b"a\xc0\x80b", 0), (1, 1)); // overlong NUL
        assert_eq!(scan_with_every_scan(b"a\xc2Zb", 0), (1, 1)); // bad continuation
        // Raw C1 / stray continuation bytes are not run material.
        assert_eq!(scan_with_every_scan(b"\x80\x81", 0), (0, 0));
        // Empty / fully-consumed.
        assert_eq!(scan_with_every_scan(b"", 0), (0, 0));
        assert_eq!(scan_with_every_scan(b"abc", 3), (3, 0));
        // ASCII stretches longer than a SIMD block, around UTF-8 and stops.
        let long = "x".repeat(70) + "\u{e9}" + &"y".repeat(40) + "\x1b[m";
        assert_eq!(scan_with_every_scan(long.as_bytes(), 0), (112, 111));
        assert_eq!(scan_with_every_scan(long.as_bytes(), 5), (112, 106));
    }

    #[test]
    fn batched_parse_matches_scalar_modulo_coalescing() {
        // Helper: coalesce Print/PrintString runs so the two modes compare.
        fn norm(actions: Vec<Action>) -> Vec<Action> {
            let mut out = Vec::new();
            let mut pending = String::new();
            for a in actions {
                match a {
                    Action::Print(c) => pending.push(c),
                    Action::PrintString(s) => pending.push_str(&s),
                    other => {
                        if !pending.is_empty() {
                            out.push(Action::PrintString(std::mem::take(&mut pending)));
                        }
                        out.push(other);
                    }
                }
            }
            if !pending.is_empty() {
                out.push(Action::PrintString(pending));
            }
            out
        }

        let inputs: &[&[u8]] = &[
            b"hello world",
            b"\x1b[1mbold\x1b[0m tail",
            "café €1 \u{1f680} 日本\x1b[31mred\x1b[0m".as_bytes(),
            b"ab\xc2\x9b31mcd",
            b"\x1b]0;title\x07body\xff\xc3\xa9",
        ];
        for bytes in inputs {
            let mut scalar = Parser::new();
            scalar.set_print_batching(false);
            let scalar_actions = norm(scalar.parse_as_vec(bytes));

            let mut batched = Parser::new();
            batched.set_print_batching(true);
            let batched_actions = norm(batched.parse_as_vec(bytes));

            assert_eq!(scalar_actions, batched_actions, "diverged for {bytes:?}");
        }
    }

    #[test]
    fn basic_bold() {
        let mut p = Parser::new();
        let actions = p.parse_as_vec(b"\x1b[1mb");
        assert_eq!(
            vec![
                Action::CSI(CSI::Sgr(Sgr::Intensity(Intensity::Bold))),
                Action::Print('b'),
            ],
            actions
        );
        assert_eq!(encode(&actions), "\x1b[1mb");
    }

    #[test]
    fn basic_bold_italic() {
        let mut p = Parser::new();
        let actions = p.parse_as_vec(b"\x1b[1;3mb");
        assert_eq!(
            vec![
                Action::CSI(CSI::Sgr(Sgr::Intensity(Intensity::Bold))),
                Action::CSI(CSI::Sgr(Sgr::Italic(true))),
                Action::Print('b'),
            ],
            actions
        );

        assert_eq!(encode(&actions), "\x1b[1m\x1b[3mb");
    }

    #[test]
    fn fancy_underline() {
        let mut p = Parser::new();

        let actions = p.parse_as_vec(b"\x1b[4:0;4:1;4:2;4:3;4:4;4:5mb");
        assert_eq!(
            vec![
                Action::CSI(CSI::Sgr(Sgr::Underline(Underline::None))),
                Action::CSI(CSI::Sgr(Sgr::Underline(Underline::Single))),
                Action::CSI(CSI::Sgr(Sgr::Underline(Underline::Double))),
                Action::CSI(CSI::Sgr(Sgr::Underline(Underline::Curly))),
                Action::CSI(CSI::Sgr(Sgr::Underline(Underline::Dotted))),
                Action::CSI(CSI::Sgr(Sgr::Underline(Underline::Dashed))),
                Action::Print('b'),
            ],
            actions
        );

        assert_eq!(
            encode(&actions),
            "\x1b[24m\x1b[4m\x1b[21m\x1b[4:3m\x1b[4:4m\x1b[4:5mb"
        );
    }

    #[test]
    fn true_color() {
        let mut p = Parser::new();

        let actions = p.parse_as_vec(b"\x1b[38:2::128:64:192mw");
        assert_eq!(
            vec![
                Action::CSI(CSI::Sgr(Sgr::Foreground(ColorSpec::TrueColor(
                    (128, 64, 192).into()
                )))),
                Action::Print('w'),
            ],
            actions
        );

        assert_eq!(encode(&actions), "\u{1b}[38:2::128:64:192mw");

        let actions = p.parse_as_vec(b"\x1b[38:2:0:255:0mw");
        assert_eq!(
            vec![
                Action::CSI(CSI::Sgr(Sgr::Foreground(ColorSpec::TrueColor(
                    (0, 255, 0).into()
                )))),
                Action::Print('w'),
            ],
            actions
        );

        let actions = p.parse_as_vec(b"\x1b[38:6:0:255:0:127mw");
        assert_eq!(
            vec![
                Action::CSI(CSI::Sgr(Sgr::Foreground(ColorSpec::TrueColor(
                    (0, 255, 0, 127).into()
                )))),
                Action::Print('w'),
            ],
            actions
        );
    }

    #[test]
    fn basic_osc() {
        let mut p = Parser::new();
        let actions = p.parse_as_vec(b"\x1b]0;hello\x07");
        assert_eq!(
            vec![Action::OperatingSystemCommand(Box::new(
                OperatingSystemCommand::SetIconNameAndWindowTitle("hello".to_owned()),
            ))],
            actions
        );
        assert_eq!(encode(&actions), "\x1b]0;hello\x1b\\");

        let actions = p.parse_as_vec(b"\x1b]532534523;hello\x07");
        assert_eq!(
            vec![Action::OperatingSystemCommand(Box::new(
                OperatingSystemCommand::Unspecified(vec![b"532534523".to_vec(), b"hello".to_vec()]),
            ))],
            actions
        );
        assert_eq!(encode(&actions), "\x1b]532534523;hello\x1b\\");
    }

    /// `;` is valid in a URI: everything after the OSC 8 params is the URI
    /// (upstream WezTerm cab251610).
    #[test]
    fn hyperlink_uri_with_semicolons() {
        let mut p = Parser::new();
        for (input, uri) in [
            (
                &b"\x1b]8;id=x;https://example.com/a;b?c=d;e\x07"[..],
                "https://example.com/a;b?c=d;e",
            ),
            (
                &b"\x1b]8;id=x;data:text/plain;base64,aGk=\x07"[..],
                "data:text/plain;base64,aGk=",
            ),
        ] {
            let actions = p.parse_as_vec(input);
            let link = crate::hyperlink::Hyperlink::new_with_id(uri, "x");
            assert_eq!(
                vec![Action::OperatingSystemCommand(Box::new(
                    OperatingSystemCommand::SetHyperlink(Some(link)),
                ))],
                actions
            );
            // Re-emission percent-encodes `;` so the URI stays unambiguous.
            assert_eq!(
                encode(&actions),
                format!("\x1b]8;id=x;{}\x1b\\", uri.replace(';', "%3B"))
            );
        }
    }

    #[test]
    fn test_emoji_title_osc() {
        let input = "\x1b]0;\u{1f915}\x07";
        let mut p = Parser::new();
        let actions = p.parse_as_vec(input.as_bytes());
        assert_eq!(
            vec![Action::OperatingSystemCommand(Box::new(
                OperatingSystemCommand::SetIconNameAndWindowTitle("\u{1f915}".to_owned()),
            ))],
            actions
        );
        assert_eq!(encode(&actions), "\x1b]0;\u{1f915}\x1b\\");
    }

    #[test]
    fn basic_esc() {
        let mut p = Parser::new();
        let actions = p.parse_as_vec(b"\x1bH");
        assert_eq!(
            vec![Action::Esc(Esc::Code(EscCode::HorizontalTabSet))],
            actions
        );
        assert_eq!(encode(&actions), "\x1bH");

        let actions = p.parse_as_vec(b"\x1b%H");
        assert_eq!(
            vec![Action::Esc(Esc::Unspecified {
                intermediate: Some(b'%'),
                control: b'H',
            })],
            actions
        );
        assert_eq!(encode(&actions), "\x1b%H");
    }

    #[test]
    fn unsupported_esc_intermediates_preserve_following_text() {
        for input in [
            b"\x1b  Xhello\x1b[1m!".as_slice(),
            b"\x1b                                Xhello\x1b[1m!".as_slice(),
        ] {
            // Exercise every boundary, including inside the escape prefix.
            for split in 0..=input.len() {
                let mut p = Parser::new();
                p.set_print_batching(false);
                let mut actions = p.parse_as_vec(&input[..split]);
                actions.extend(p.parse_as_vec(&input[split..]));
                let mut expected = Parser::new();
                expected.set_print_batching(false);
                assert_eq!(actions, expected.parse_as_vec(b"hello\x1b[1m!"));
                let encoded = encode(&actions);
                let mut replay = Parser::new();
                replay.set_print_batching(false);
                assert_eq!(actions, replay.parse_as_vec(encoded.as_bytes()));
            }
        }
    }

    /// ft-yccm0.3.2.1: a handler that takes every hot action itself sees
    /// the stream `parse` produces, in order, whole and byte by byte, under
    /// every parser mode. `&mut dyn Handler` works too.
    #[test]
    fn parse_with_hands_a_handler_the_same_stream_as_parse() {
        #[derive(Default)]
        struct Recorder(Vec<Action>);

        impl Handler for Recorder {
            fn action(&mut self, action: Action) {
                self.0.push(action);
            }
            fn print(&mut self, c: char) {
                self.0.push(Action::Print(c));
            }
            fn print_str(&mut self, text: &str) {
                self.0.push(Action::PrintString(text.to_string()));
            }
            fn print_ascii_run(&mut self, run: &str) {
                self.0.push(Action::PrintString(run.to_string()));
            }
            fn control(&mut self, code: ControlCode) {
                self.0.push(Action::Control(code));
            }
            fn csi(&mut self, csi: CSI) {
                self.0.push(Action::CSI(csi));
            }
            fn esc(&mut self, esc: Esc) {
                self.0.push(Action::Esc(esc));
            }
            fn sgr(&mut self, sgrs: &[Sgr]) {
                self.0
                    .extend(sgrs.iter().cloned().map(|sgr| Action::CSI(CSI::Sgr(sgr))));
            }
        }

        let inputs: &[&[u8]] = &[
            b"hello world",
            "caf\u{e9} \u{20ac}1 \u{1f680} \u{65e5}\x1b[31mred\x1b[0m".as_bytes(),
            b"\x1b[1;4;38;5;196;48;2;1;2;3mX\x1b[m\r\n\x07tail",
            b"\x1b]0;title\x07body\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\",
            b"\x1bP$qm\x1b\\\x1bPq#0;2;0;0;0#0~~\x1b\\after",
            b"\x1b(0lqk\x1b(B\x1b7\x1b8\x1bMab",
            b"a\xe4\x1b[1mb\x9bz\x1b[?2026h\x1b[?2026l",
            "\x1b[38;5;196m\x1b[48;5;21m\u{1f600}\x1b[38;5;196m\x1b[48;5;21m\u{e9}\x1b[5;9H\x1b[K"
                .as_bytes(),
        ];
        for &batching in &[false, true] {
            for &table in &[false, true] {
                let configure = |parser: &mut Parser| {
                    parser.set_print_batching(batching);
                    parser.set_table_dispatch(table);
                };
                for bytes in inputs {
                    let mut reference = Parser::new();
                    configure(&mut reference);
                    let expected = reference.parse_as_vec(bytes);

                    let mut parser = Parser::new();
                    configure(&mut parser);
                    let mut recorder = Recorder::default();
                    parser.parse_with(bytes, &mut recorder);
                    assert_eq!(recorder.0, expected);

                    // Byte at a time the runs split differently, so compare
                    // with `parse` fed the same way.
                    let mut reference = Parser::new();
                    configure(&mut reference);
                    let mut expected = Vec::new();
                    let mut parser = Parser::new();
                    configure(&mut parser);
                    let mut recorder = Recorder::default();
                    for byte in bytes.iter() {
                        let byte = core::slice::from_ref(byte);
                        reference.parse(byte, |action| expected.push(action));
                        parser.parse_with(byte, &mut recorder as &mut dyn Handler);
                    }
                    assert_eq!(recorder.0, expected);
                }
            }
        }
    }

    /// ft-yccm0.3.2.3: `decode_utf8_char` takes exactly the first character
    /// `core::str::from_utf8` accepts, minus ASCII and the C1 controls.
    /// Checked over every two- and three-byte input, and every four-byte
    /// input with a representative third and fourth byte. Plain `assert!`
    /// keeps millions of checks fast; k9 formats every call.
    #[test]
    fn decode_utf8_char_matches_std_on_every_short_input() {
        fn expected(bytes: &[u8]) -> Option<(char, usize)> {
            let valid = match core::str::from_utf8(bytes) {
                Ok(text) => text,
                Err(error) => core::str::from_utf8(&bytes[..error.valid_up_to()]).ok()?,
            };
            let c = valid.chars().next()?;
            (!c.is_ascii() && !('\u{80}'..='\u{9f}').contains(&c)).then_some((c, c.len_utf8()))
        }
        const EDGES: [u8; 9] = [0x00, 0x7f, 0x80, 0x8f, 0x90, 0xa0, 0xbf, 0xc0, 0xff];
        for lead in 0..=0xffu8 {
            let one = [lead];
            assert!(decode_utf8_char(&one) == expected(&one), "{one:02x?}");
            for second in 0..=0xffu8 {
                let two = [lead, second];
                assert!(decode_utf8_char(&two) == expected(&two), "{two:02x?}");
                // An ASCII lead never decodes; its longer inputs add nothing.
                if lead < 0x80 {
                    continue;
                }
                for third in 0..=0xffu8 {
                    let three = [lead, second, third];
                    assert!(decode_utf8_char(&three) == expected(&three), "{three:02x?}");
                }
                for third in EDGES {
                    for fourth in EDGES {
                        let four = [lead, second, third, fourth];
                        assert!(decode_utf8_char(&four) == expected(&four), "{four:02x?}");
                    }
                }
            }
        }
        // Every valid four-byte sequence's extremes.
        for text in ["\u{10000}", "\u{1f600}", "\u{10ffff}"] {
            let bytes = text.as_bytes();
            assert!(decode_utf8_char(bytes) == expected(bytes), "{text:?}");
        }
    }

    /// Text pieces for the run equivalence property: ASCII, valid UTF-8 of
    /// every length (Latin-1 supplement, C1 controls encoded in UTF-8, CJK,
    /// emoji), controls and DEL, and malformed UTF-8 (stray continuations,
    /// overlongs, surrogates, values past U+10FFFF, truncated sequences).
    fn arb_run_piece() -> impl proptest::strategy::Strategy<Value = Vec<u8>> {
        use proptest::prelude::*;
        const PIECES: &[&[u8]] = &[
            b"a",
            b"text with spaces ",
            b"0123456789abcdef0123456789",
            "\u{e9}".as_bytes(),
            "\u{a0}".as_bytes(),
            "\u{85}".as_bytes(),
            "\u{9b}".as_bytes(),
            "\u{20ac}".as_bytes(),
            "\u{2500}\u{2502}\u{250c}".as_bytes(),
            "\u{4e2d}\u{6587}".as_bytes(),
            "\u{1f600}".as_bytes(),
            "\u{fe0f}\u{200d}".as_bytes(),
            b"\x1b",
            b"\r\n",
            b"\x7f",
            b"\x80",
            b"\xbf",
            b"\xc0\x80",
            b"\xc1\xbf",
            b"\xe0\x80\x80",
            b"\xed\xa0\x80",
            b"\xf0\x80\x80\x80",
            b"\xf4\x90\x80\x80",
            b"\xf8\x88\x80\x80\x80",
            b"\xe2\x82",
            b"\xf0\x9f\x98",
            b"\xc3",
            b"\xff",
        ];
        proptest::collection::vec(proptest::sample::select(PIECES), 0..24)
            .prop_map(|pieces| pieces.concat())
    }

    proptest::proptest! {
        /// ft-yccm0.3.2.3: the one-validation run is the oracle's run, with
        /// every `AsciiScan`, from every byte that can start a run.
        #[test]
        fn ground_run_bulk_matches_the_oracle(bytes in arb_run_piece()) {
            for scan in AsciiScan::ALL {
                // One cache across increasing starts, as within a parse call.
                let mut shared = RunExtents::default();
                for (start, &byte) in bytes.iter().enumerate() {
                    if !can_start_printable_run(byte) {
                        continue;
                    }
                    let oracle = ground_run_scalar(&bytes, start, AsciiScan::Scalar);
                    proptest::prop_assert_eq!(
                        ground_run_bulk(&bytes, start, scan, &mut RunExtents::default()),
                        oracle.clone(),
                        "{:?} from {}",
                        scan,
                        start
                    );
                    proptest::prop_assert_eq!(
                        ground_run_bulk(&bytes, start, scan, &mut shared),
                        oracle,
                        "{:?} from {} with a shared cache",
                        scan,
                        start
                    );
                }
            }
        }
    }

    #[test]
    fn csi_fast_path_default_policy_is_on_unless_falsey() {
        assert!(csi_fast_path_default_for_value(None));
        for truthy in ["1", "on", "true", "yes"] {
            assert!(csi_fast_path_default_for_value(Some(truthy)), "{truthy}");
        }
        for falsey in ["", "0", "false", "OFF", " no "] {
            assert!(!csi_fast_path_default_for_value(Some(falsey)), "{falsey:?}");
        }
        let mut parser = Parser::new();
        parser.set_csi_fast_path(false);
        assert!(!parser.csi_fast_path());
        parser.set_csi_fast_path(true);
        assert!(parser.csi_fast_path());
    }

    /// One byte of a CSI sequence's body, biased toward what the scanner
    /// branches on: digits (long runs overflow), separators, markers,
    /// intermediates, finals, and bytes that abort or interrupt a sequence.
    fn arb_csi_body_byte() -> impl proptest::strategy::Strategy<Value = u8> {
        use proptest::prelude::*;
        prop_oneof![
            8 => proptest::sample::select(b"0123456789".to_vec()),
            3 => proptest::sample::select(b";:".to_vec()),
            1 => proptest::sample::select(b"?<=>".to_vec()),
            1 => proptest::sample::select(b" !$*".to_vec()),
            2 => proptest::sample::select(b"mHJKABCDrhlu@`~".to_vec()),
            1 => proptest::sample::select(vec![0x07, 0x0a, 0x18, 0x1b, 0x7f, 0x9b, 0xc3, 0xe2]),
        ]
    }

    proptest::proptest! {
        /// ft-yccm0.3.2.4: whenever the CSI fast path's scanner takes a
        /// sequence, the state machine fed the same bytes from ground
        /// dispatches exactly that sequence, with the same parameters and
        /// untruncated, and is back in ground.
        #[test]
        fn scan_csi_reproduces_the_state_machine_dispatch(
            body in proptest::collection::vec(arb_csi_body_byte(), 0..40),
        ) {
            let mut bytes = b"\x1b[".to_vec();
            bytes.extend_from_slice(&body);
            let mut params = [CsiParam::Integer(0); CSI_FAST_MAX_PARAMS];
            if let Some((fin, count)) = scan_csi(&bytes, 0, &mut params) {
                let mut machine = VTParser::new();
                let mut actor = vtparse::CollectingVTActor::default();
                machine.parse(&bytes[..=fin], &mut actor);
                proptest::prop_assert!(machine.is_ground());
                let expected = vec![vtparse::VTAction::CsiDispatch {
                    params: params[..count].to_vec(),
                    parameters_truncated: false,
                    byte: bytes[fin],
                }];
                proptest::prop_assert_eq!(actor.into_vec(), expected);
            }
        }
    }

    /// ft-yccm0.3.2.4: a sequence with the last SGR's parameter bytes is
    /// answered from the cache, and nothing else is.
    #[test]
    fn last_sgr_cache_answers_only_the_same_parameter_bytes() {
        use crate::color::ColorSpec;
        let mut parser = Parser::new();
        parser.set_print_batching(true);
        parser.set_csi_fast_path(true);
        assert_eq!(
            parser.parse_as_vec(b"\x1b[38;5;196m"),
            vec![Action::CSI(CSI::Sgr(Sgr::Foreground(
                ColorSpec::PaletteIndex(196)
            )))]
        );
        assert_eq!(
            &parser.last_sgr.key[..parser.last_sgr.key_len],
            b"38;5;196".as_slice()
        );

        // Poison the entry with a setting these bytes never decode to: only
        // a hit can return it.
        let poison = Sgr::Inverse(true);
        parser
            .last_sgr
            .store(b"38;5;196", core::slice::from_ref(&poison));
        assert_eq!(
            parser.parse_as_vec(b"\x1b[38;5;196mX\x1b[38;5;196m"),
            vec![
                Action::CSI(CSI::Sgr(poison.clone())),
                Action::Print('X'),
                Action::CSI(CSI::Sgr(poison.clone())),
            ]
        );

        // Another final, a longer list sharing the prefix and a shorter one
        // all miss, and decode as the state machine path does.
        for bytes in [
            b"\x1b[38;5;196H".as_slice(),
            b"\x1b[38;5;196;1m",
            b"\x1b[38;5;19mZ",
            b"\x1b[38;5;196",
        ] {
            parser
                .last_sgr
                .store(b"38;5;196", core::slice::from_ref(&poison));
            let mut reference = Parser::new();
            reference.set_csi_fast_path(false);
            assert_eq!(
                parser.parse_as_vec(bytes),
                reference.parse_as_vec(bytes),
                "{:?}",
                bytes
            );
        }
    }

    /// ft-yccm0.3.2.4: the operator's T0 shape. Each pair of SGR sequences
    /// reaches the handler as one run, and the character between escapes as
    /// one `Print`, decoded without the state machine.
    #[test]
    fn consecutive_sgr_sequences_reach_the_handler_as_one_run() {
        use crate::color::ColorSpec;
        #[derive(Default)]
        struct Runs {
            runs: Vec<Vec<Sgr>>,
            other: Vec<Action>,
        }
        impl Handler for Runs {
            fn action(&mut self, action: Action) {
                self.other.push(action);
            }
            fn sgr(&mut self, sgrs: &[Sgr]) {
                self.runs.push(sgrs.to_vec());
            }
        }

        let bytes =
            "\x1b[38;5;196m\x1b[48;5;21m\u{1f600}\x1b[38;5;7m\x1b[48;2;1;2;3m\u{1f680}".as_bytes();
        let mut parser = Parser::new();
        parser.set_print_batching(true);
        parser.set_csi_fast_path(true);
        let mut runs = Runs::default();
        parser.parse_with(bytes, &mut runs);
        let palette = ColorSpec::PaletteIndex;
        let rgb: ColorSpec = crate::color::RgbColor::new_8bpc(1, 2, 3).into();
        assert_eq!(
            runs.runs,
            vec![
                vec![Sgr::Foreground(palette(196)), Sgr::Background(palette(21))],
                vec![Sgr::Foreground(palette(7)), Sgr::Background(rgb)],
            ]
        );
        assert_eq!(
            runs.other,
            vec![Action::Print('\u{1f600}'), Action::Print('\u{1f680}')]
        );

        // Forty SGR sequences in a row overflow one run; the stream is still
        // the state machine path's.
        let many = "\x1b[1m\x1b[38;5;9m".repeat(20) + "x";
        let mut fast = Parser::new();
        fast.set_csi_fast_path(true);
        let mut slow = Parser::new();
        slow.set_csi_fast_path(false);
        assert_eq!(
            fast.parse_as_vec(many.as_bytes()),
            slow.parse_as_vec(many.as_bytes())
        );
    }

    #[test]
    fn overflowed_esc_dispatch_rejects_even_a_short_retained_prefix() {
        let p = Parser::new();
        let mut actions = Vec::new();
        let mut collector = ActionCollector(|action| actions.push(action));
        let mut state = p.state.borrow_mut();
        let mut performer = Performer {
            handler: &mut collector,
            state: &mut state,
        };
        performer.esc_dispatch(&[], &[], true, b'X');
        performer.esc_dispatch(&[], b"%", true, b'H');
        assert!(actions.is_empty());
    }

    #[test]
    fn soft_reset() {
        let mut p = Parser::new();
        let actions = p.parse_as_vec(b"\x1b[!p");
        assert_eq!(
            vec![Action::CSI(CSI::Device(Box::new(
                crate::csi::Device::SoftReset
            )))],
            actions
        );
        assert_eq!(encode(&actions), "\x1b[!p");
    }

    #[test]
    fn tmux_title_escape() {
        let mut p = Parser::new();
        // The expected action stream below proves batched representation,
        // independent of the process-wide default/escape-hatch setting.
        p.set_print_batching(true);
        let actions = p.parse_as_vec(b"\x1bktitle\x1b\\");
        assert_eq!(
            vec![
                Action::Esc(Esc::Code(EscCode::TmuxTitle)),
                Action::PrintString("title".to_string()),
                Action::Esc(Esc::Code(EscCode::StringTerminator)),
            ],
            actions
        );
    }

    #[test]
    fn parse_first_streaming_incomplete_then_complete_osc() {
        let mut p = Parser::new();

        assert_eq!(None, p.parse_first(b"\x1b]0;hel"));

        let chunk = b"lo\x07X";
        let (action, consumed) = p
            .parse_first(chunk)
            .expect("expected OSC action once BEL terminator is seen");
        assert_eq!(
            Action::OperatingSystemCommand(Box::new(
                OperatingSystemCommand::SetIconNameAndWindowTitle("hello".to_owned()),
            )),
            action
        );
        assert_eq!(3, consumed);

        assert_eq!(
            Some((Action::Print('X'), 1)),
            p.parse_first(&chunk[consumed..])
        );
    }

    #[test]
    fn parse_first_streaming_st_terminator_split_across_chunks() {
        let mut p = Parser::new();

        let (action, consumed) = p
            .parse_first(b"\x1b]0;hello\x1b")
            .expect("first-action API dispatches OSC on ESC");
        assert_eq!(
            Action::OperatingSystemCommand(Box::new(
                OperatingSystemCommand::SetIconNameAndWindowTitle("hello".to_owned()),
            )),
            action
        );
        assert_eq!(10, consumed);
        let chunk = b"\\X";
        let (action, consumed) = p.parse_first(chunk).expect("ST completes separately");
        assert_eq!(Action::Esc(Esc::Code(EscCode::StringTerminator)), action);
        assert_eq!(1, consumed);
        assert_eq!(
            Some((Action::Print('X'), 1)),
            p.parse_first(&chunk[consumed..])
        );
    }

    #[test]
    fn parse_first_as_vec_stops_at_ground_boundary() {
        let mut p = Parser::new();
        let data = b"\x1b[1mB";
        let (actions, consumed) = p
            .parse_first_as_vec(data)
            .expect("bounded sequence")
            .expect("expected first completed sequence");

        assert_eq!(
            vec![Action::CSI(CSI::Sgr(Sgr::Intensity(Intensity::Bold)))],
            actions
        );
        assert_eq!(4, consumed);
        assert_eq!(
            Some((vec![Action::Print('B')], 1)),
            p.parse_first_as_vec(&data[consumed..]).unwrap()
        );
    }

    #[test]
    fn parse_first_as_vec_streaming_st_terminator_split_across_chunks() {
        let mut p = Parser::new();

        assert_eq!(None, p.parse_first_as_vec(b"\x1b]0;hello\x1b").unwrap());

        let chunk = b"\\X";
        let (actions, consumed) = p
            .parse_first_as_vec(chunk)
            .expect("bounded sequence")
            .expect("expected completed OSC+ST sequence");

        assert_eq!(
            vec![
                Action::OperatingSystemCommand(Box::new(
                    OperatingSystemCommand::SetIconNameAndWindowTitle("hello".to_owned()),
                )),
                Action::Esc(Esc::Code(EscCode::StringTerminator)),
            ],
            actions
        );
        assert_eq!(1, consumed);
        assert_eq!(
            Some((vec![Action::Print('X')], 1)),
            p.parse_first_as_vec(&chunk[consumed..]).unwrap()
        );
    }

    #[test]
    fn first_sequence_actions_survive_every_chunk_split() {
        let input = b"\x1b]8;id=fixture;https://example.com\x1b\\X";
        let mut reference = Parser::new();
        reference.set_print_batching(false);
        let expected = reference.parse_as_vec(input);
        for split in 0..=input.len() {
            let mut parser = Parser::new();
            let mut actual = Vec::new();
            for chunk in [&input[..split], &input[split..]] {
                let mut offset = 0;
                while let Some((actions, consumed)) =
                    parser.parse_first_as_vec(&chunk[offset..]).unwrap()
                {
                    actual.extend(actions);
                    offset += consumed;
                }
            }
            assert_eq!(actual, expected, "split {split}");
            assert!(parser.recovery_ground_boundary().is_some());
            assert!(parser.parse_first_as_vec(b"").unwrap().is_none());
        }
    }

    #[test]
    fn retained_sequence_actions_survive_api_switch_exactly_once() {
        let mut parser = Parser::new();
        assert!(
            parser
                .parse_first_as_vec(b"\x1b]0;hello\x1b")
                .unwrap()
                .is_none()
        );
        assert!(parser.recovery_ground_boundary().is_none());
        assert_eq!(
            parser.parse_first(b""),
            Some((
                Action::OperatingSystemCommand(Box::new(
                    OperatingSystemCommand::SetIconNameAndWindowTitle("hello".to_owned())
                )),
                0
            ))
        );
        assert_eq!(
            parser.parse_as_vec(b"\\"),
            vec![Action::Esc(Esc::Code(EscCode::StringTerminator))]
        );
        assert!(parser.recovery_ground_boundary().is_some());

        let mut parser = Parser::new();
        assert!(
            parser
                .parse_first_as_vec(b"\x1b]0;hello\x1b")
                .unwrap()
                .is_none()
        );
        let actions = parser.parse_as_vec(b"\\");
        assert_eq!(actions.len(), 2);
        assert!(
            matches!(&actions[0], Action::OperatingSystemCommand(command)
            if **command == OperatingSystemCommand::SetIconNameAndWindowTitle("hello".to_owned()))
        );
        assert_eq!(
            actions[1],
            Action::Esc(Esc::Code(EscCode::StringTerminator))
        );
        assert!(parser.parse_as_vec(b"").is_empty());
    }

    #[test]
    fn first_sequence_action_limit_checks_multi_action_final_byte() {
        for exceeds_limit in [false, true] {
            let osc_count = MAX_FIRST_SEQUENCE_ACTIONS - 2 + usize::from(exceeds_limit);
            let mut input = vec![0x1b];
            for _ in 0..osc_count {
                input.extend_from_slice(b"]0;x\x1b");
            }
            input.extend_from_slice(b"[1;3m");
            let mut oracle = Parser::new();
            let expected = oracle.parse_as_vec(&input);
            assert_eq!(expected.len(), osc_count + 2);

            let mut parser = Parser::new();
            let final_byte = input.pop().unwrap();
            assert!(parser.parse_first_as_vec(&input).unwrap().is_none());
            let result = parser.parse_first_as_vec(&[final_byte, b'X']);
            let (actions, consumed) = if exceeds_limit {
                let error = result.unwrap_err();
                (error.actions, error.consumed)
            } else {
                result.unwrap().unwrap()
            };
            assert_eq!(actions, expected);
            assert_eq!(consumed, 1);
            assert!(parser.pending_actions.is_empty());
            assert!(parser.recovery_ground_boundary().is_some());
            assert_eq!(parser.parse_as_vec(b"X"), vec![Action::Print('X')]);
            assert!(parser.parse_as_vec(b"").is_empty());
        }
    }

    #[test]
    fn first_sequence_action_limit_returns_lossless_partial_batch() {
        let mut parser = Parser::new();
        let mut input = vec![0x1b];
        for _ in 0..MAX_FIRST_SEQUENCE_ACTIONS {
            input.extend_from_slice(b"]0;x\x1b");
        }
        let mut single_call = Parser::new();
        let mut terminated = input.clone();
        terminated.push(b'\\');
        let single_limit = single_call.parse_first_as_vec(&terminated).unwrap_err();
        assert_eq!(single_limit.consumed, input.len());
        assert_eq!(single_limit.actions.len(), MAX_FIRST_SEQUENCE_ACTIONS);
        assert!(parser.parse_first_as_vec(&input).unwrap().is_none());
        let limited = parser.parse_first_as_vec(b"\\").unwrap_err();
        assert_eq!(limited.consumed, 0);
        assert_eq!(limited.actions.len(), MAX_FIRST_SEQUENCE_ACTIONS);
        assert!(
            limited.actions.iter().all(|action| matches!(action,
            Action::OperatingSystemCommand(command)
                if **command == OperatingSystemCommand::SetIconNameAndWindowTitle("x".to_owned())))
        );
        assert!(parser.pending_actions.is_empty());
        assert!(parser.recovery_ground_boundary().is_none());
        let (rest, consumed) = parser.parse_first_as_vec(b"\\").unwrap().unwrap();
        assert_eq!(consumed, 1);
        assert_eq!(
            rest,
            vec![Action::Esc(Esc::Code(EscCode::StringTerminator))]
        );
        assert!(parser.recovery_ground_boundary().is_some());
    }

    #[test]
    fn pending_action_queue_survives_single_byte_chunks_without_repacking() {
        let mut parser = Parser::new();
        let mut prefix = vec![0x1b];
        for _ in 0..512 {
            prefix.extend_from_slice(b"]0;x\x1b");
        }
        prefix.extend_from_slice(b"]0;");
        assert!(parser.parse_first_as_vec(&prefix).unwrap().is_none());
        let capacity = parser.pending_actions.capacity();
        let first_action = parser.pending_actions.front().unwrap() as *const Action;
        for _ in 0..256 {
            assert!(parser.parse_first_as_vec(b"y").unwrap().is_none());
            assert_eq!(parser.pending_actions.len(), 512);
            assert_eq!(parser.pending_actions.capacity(), capacity);
            assert_eq!(
                parser.pending_actions.front().unwrap() as *const Action,
                first_action
            );
        }
        let (actions, consumed) = parser.parse_first_as_vec(b"\x07").unwrap().unwrap();
        assert_eq!(consumed, 1);
        assert_eq!(actions.len(), 513);
        assert!(
            matches!(&actions[512], Action::OperatingSystemCommand(command)
            if **command == OperatingSystemCommand::SetIconNameAndWindowTitle("y".repeat(256)))
        );
        assert!(parser.parse_as_vec(b"").is_empty());
        assert!(parser.recovery_ground_boundary().is_some());
    }

    #[test]
    fn first_sequence_byte_limit_applies_across_chunks() {
        let mut parser = Parser::new();
        let mut input = b"\x1b]0;".to_vec();
        input.resize(MAX_FIRST_SEQUENCE_BYTES, b'x');
        let split = input.len() / 2;
        assert!(
            parser
                .parse_first_as_vec(&input[..split])
                .unwrap()
                .is_none()
        );
        assert!(
            parser
                .parse_first_as_vec(&input[split..])
                .unwrap()
                .is_none()
        );
        let limited = parser.parse_first_as_vec(b"\x07").unwrap_err();
        assert_eq!(limited.consumed, 0);
        assert!(limited.actions.is_empty());
        assert!(parser.recovery_ground_boundary().is_none());
        let (rest, consumed) = parser.parse_first_as_vec(b"\x07").unwrap().unwrap();
        assert_eq!(consumed, 1);
        assert_eq!(rest.len(), 1);
        assert!(matches!(&rest[0], Action::OperatingSystemCommand(command)
            if matches!(&**command, OperatingSystemCommand::SetIconNameAndWindowTitle(title)
                if title.len() == MAX_FIRST_SEQUENCE_BYTES - 4)));
        assert!(parser.recovery_ground_boundary().is_some());
    }

    #[test]
    fn first_sequence_limit_error_does_not_format_semantic_payload() {
        let error = FirstSequenceLimitExceeded {
            actions: vec![Action::PrintString("private-parser-marker".to_string())],
            consumed: 7,
        };
        assert_eq!(
            format!("{error:?}"),
            "FirstSequenceLimitExceeded { action_count: 1, consumed: 7 }"
        );
        assert_eq!(
            error.to_string(),
            "first sequence collection limit exceeded"
        );
        assert!(!format!("{error:?} {error}").contains("private-parser-marker"));
    }

    fn round_trip_parse(s: &str) -> Vec<Action> {
        let mut p = Parser::new();
        let actions = p.parse_as_vec(s.as_bytes());
        assert_eq!(s, encode(&actions), "actions: {actions:?}");
        actions
    }

    fn parse_as(s: &str, expected: &str) -> Vec<Action> {
        let mut p = Parser::new();
        let actions = p.parse_as_vec(s.as_bytes());
        assert_eq!(expected, encode(&actions), "actions: {actions:?}");
        actions
    }

    #[test]
    fn xtgettcap() {
        assert_eq!(
            round_trip_parse("\x1bP+q544e\x1b\\"),
            vec![
                Action::XtGetTcap(vec!["TN".to_string()]),
                Action::Esc(Esc::Code(EscCode::StringTerminator)),
            ]
        );
    }

    #[test]
    fn xtgettcap_multiple_names_and_raw_fallback() {
        let mut p = Parser::new();
        let actions = p.parse_as_vec(b"\x1bP+q544e;XYZ\x1b\\");

        assert_eq!(
            vec![
                Action::XtGetTcap(vec!["TN".to_string(), "XYZ".to_string()]),
                Action::Esc(Esc::Code(EscCode::StringTerminator)),
            ],
            actions
        );
        // Raw names are normalized to hex in the formatter.
        assert_eq!(encode(&actions), "\x1bP+q544e;58595a\x1b\\");
    }

    #[test]
    fn xtgettcap_empty_request_does_not_emit_empty_name() {
        assert_eq!(
            round_trip_parse("\x1bP+q\x1b\\"),
            vec![
                Action::XtGetTcap(vec![]),
                Action::Esc(Esc::Code(EscCode::StringTerminator)),
            ]
        );
    }

    #[test]
    fn xtgettcap_trailing_separator_ignores_empty_name() {
        let mut p = Parser::new();
        let actions = p.parse_as_vec(b"\x1bP+q544e;\x1b\\");
        assert_eq!(
            vec![
                Action::XtGetTcap(vec!["TN".to_string()]),
                Action::Esc(Esc::Code(EscCode::StringTerminator)),
            ],
            actions
        );
        // The action model intentionally carries no empty capability name, so
        // encoding produces the canonical request without a trailing separator.
        assert_eq!("\x1bP+q544e\x1b\\", encode(&actions));
    }

    #[test]
    fn short_dcs_round_trip() {
        let mut p = Parser::new();
        let actions = p.parse_as_vec(b"\x1bP$qm\x1b\\");

        assert_eq!(
            vec![
                Action::DeviceControl(DeviceControlMode::ShortDeviceControl(Box::new(
                    ShortDeviceControl {
                        params: vec![],
                        intermediates: vec![b'$'],
                        byte: b'q',
                        data: b"m".to_vec(),
                    },
                ))),
                Action::Esc(Esc::Code(EscCode::StringTerminator)),
            ],
            actions
        );
        assert_eq!(encode(&actions), "\x1bP$qm\x1b\\");
    }

    #[test]
    fn overlong_short_dcs_is_discarded() {
        let mut p = Parser::new();
        let mut seq = Vec::with_capacity(MAX_SHORT_DCS_BYTES + 512);
        seq.extend_from_slice(b"\x1bP$q");
        seq.extend(std::iter::repeat_n(b'x', MAX_SHORT_DCS_BYTES + 128));
        seq.extend_from_slice(b"\x1b\\");
        seq.extend_from_slice(b"\x1bP$qOK\x1b\\");

        let actions = p.parse_as_vec(&seq);
        let mut short_dcs = Vec::new();
        for action in actions {
            if let Action::DeviceControl(DeviceControlMode::ShortDeviceControl(dcs)) = action {
                short_dcs.push(*dcs);
            }
        }

        assert_eq!(1, short_dcs.len());
        assert_eq!(b"OK".to_vec(), short_dcs[0].data);
        assert_eq!(
            p.take_string_sequence_error(),
            Some(crate::StringSequenceError::LimitExceeded {
                kind: crate::StringSequenceKind::ShortDeviceControl,
                maximum: MAX_SHORT_DCS_BYTES,
            })
        );
        assert_eq!(p.take_string_sequence_error(), None);
    }

    #[test]
    fn overlong_xtgettcap_current_suppresses_the_whole_request_and_recovers() {
        let mut p = Parser::new();
        let mut seq = Vec::with_capacity(MAX_TCAP_CURRENT_BYTES + 512);
        // DCS +q  starts XtGetTcap mode
        seq.extend_from_slice(b"\x1bP+q");
        // Push more bytes than the per-name cap without a semicolon separator
        seq.extend(std::iter::repeat_n(b'4', MAX_TCAP_CURRENT_BYTES + 128));
        // A later bounded name in the same request must not be reinterpreted as
        // an independent command after the overlong prefix is discarded.
        seq.extend_from_slice(b";544e");
        // Terminate the DCS
        seq.extend_from_slice(b"\x1b\\");
        // Parse a subsequent well-formed XtGetTcap to verify recovery
        seq.extend_from_slice(b"\x1bP+q544e\x1b\\");

        let actions = p.parse_as_vec(&seq);
        let mut tcap_results = Vec::new();
        for action in &actions {
            if let Action::XtGetTcap(names) = action {
                tcap_results.push(names.clone());
            }
        }

        // A resource violation invalidates the entire first request: emitting
        // its accepted suffix would expose a partial attacker-controlled
        // command to streaming callback users. The second request proves that
        // the rejection state resets at the DCS boundary.
        assert_eq!(vec![vec!["TN".to_string()]], tcap_results);
        assert_eq!(
            p.take_string_sequence_error(),
            Some(crate::StringSequenceError::LimitExceeded {
                kind: crate::StringSequenceKind::XtGetTcap,
                maximum: MAX_TCAP_CURRENT_BYTES,
            })
        );
        assert_eq!(p.take_string_sequence_error(), None);
    }

    #[test]
    fn overlong_xtgettcap_name_count_suppresses_the_whole_request() {
        let mut p = Parser::new();
        let mut seq = Vec::new();
        // DCS +q starts XtGetTcap mode
        seq.extend_from_slice(b"\x1bP+q");
        // Push MAX_TCAP_NAMES + 10 semicolon-separated single-byte names
        for i in 0..(MAX_TCAP_NAMES + 10) {
            if i > 0 {
                seq.push(b';');
            }
            // Each name is a single hex digit
            seq.push(b'4');
        }
        // Terminate
        seq.extend_from_slice(b"\x1b\\");
        // Follow with a well-formed XtGetTcap
        seq.extend_from_slice(b"\x1bP+q544e\x1b\\");

        let actions = p.parse_as_vec(&seq);
        let mut tcap_results = Vec::new();
        for action in &actions {
            if let Action::XtGetTcap(names) = action {
                tcap_results.push(names.clone());
            }
        }

        assert_eq!(vec![vec!["TN".to_string()]], tcap_results);
        assert_eq!(
            p.take_string_sequence_error(),
            Some(crate::StringSequenceError::LimitExceeded {
                kind: crate::StringSequenceKind::XtGetTcap,
                maximum: MAX_TCAP_NAMES,
            })
        );
        assert_eq!(p.take_string_sequence_error(), None);
    }

    #[test]
    fn aggregate_xtgettcap_name_bytes_are_capped_and_parser_recovers() {
        let mut p = Parser::new();
        let first_name_bytes = MAX_TCAP_TOTAL_BYTES;
        let mut seq = Vec::with_capacity(MAX_TCAP_TOTAL_BYTES + 512);
        seq.extend_from_slice(b"\x1bP+q");
        seq.extend(std::iter::repeat_n(b'4', first_name_bytes));
        // The exact aggregate boundary is valid. The first byte of the second
        // name would exceed it, so that name and every remaining name in this
        // DCS must be ignored without truncating anything into a new command.
        seq.extend_from_slice(b";544e;434f");
        seq.extend_from_slice(b"\x1b\\");
        // A fresh request must not inherit the discard state.
        seq.extend_from_slice(b"\x1bP+q544e\x1b\\");

        let actions = p.parse_as_vec(&seq);
        let tcap_results: Vec<_> = actions
            .iter()
            .filter_map(|action| match action {
                Action::XtGetTcap(names) => Some(names),
                _ => None,
            })
            .collect();

        assert_eq!(1, tcap_results.len());
        assert_eq!(&["TN".to_string()], tcap_results[0].as_slice());
        assert_eq!(
            p.take_string_sequence_error(),
            Some(crate::StringSequenceError::LimitExceeded {
                kind: crate::StringSequenceKind::XtGetTcap,
                maximum: MAX_TCAP_TOTAL_BYTES,
            })
        );
        assert_eq!(p.take_string_sequence_error(), None);
    }

    #[test]
    fn generic_dcs_emits_enter_data_exit() {
        let mut p = Parser::new();
        let actions = p.parse_as_vec(b"\x1bP1;2zABC\x1b\\");

        assert_eq!(
            vec![
                Action::DeviceControl(DeviceControlMode::Enter(Box::new(EnterDeviceControlMode {
                    params: vec![1, 2],
                    intermediates: vec![],
                    byte: b'z',
                    ignored_extra_intermediates: false,
                }))),
                Action::DeviceControl(DeviceControlMode::Data(b'A')),
                Action::DeviceControl(DeviceControlMode::Data(b'B')),
                Action::DeviceControl(DeviceControlMode::Data(b'C')),
                Action::DeviceControl(DeviceControlMode::Exit),
                Action::Esc(Esc::Code(EscCode::StringTerminator)),
            ],
            actions
        );
        assert_eq!(encode(&actions), "\x1bP1;2zABC\x1b\\");
    }

    #[test]
    fn bidi_modes() {
        assert_eq!(
            round_trip_parse("\x1b[1 k"),
            vec![Action::CSI(CSI::SelectCharacterPath(
                CharacterPath::LeftToRightOrTopToBottom,
                0
            ))]
        );
        assert_eq!(
            round_trip_parse("\x1b[2;1 k"),
            vec![Action::CSI(CSI::SelectCharacterPath(
                CharacterPath::RightToLeftOrBottomToTop,
                1
            ))]
        );
    }

    #[test]
    fn xterm_key() {
        assert_eq!(
            round_trip_parse("\x1b[>4;2m"),
            vec![Action::CSI(CSI::Mode(Mode::XtermKeyMode {
                resource: XtermKeyModifierResource::OtherKeys,
                value: Some(2),
            }))]
        );
        assert_eq!(
            round_trip_parse("\x1b[>4;m"),
            vec![Action::CSI(CSI::Mode(Mode::XtermKeyMode {
                resource: XtermKeyModifierResource::OtherKeys,
                value: None,
            }))]
        );
    }

    #[test]
    fn window() {
        assert_eq!(
            round_trip_parse("\x1b[22;2t"),
            vec![Action::CSI(CSI::Window(Box::new(Window::PushWindowTitle)))]
        );
    }

    #[test]
    fn checksum_area() {
        assert_eq!(
            round_trip_parse("\x1b[1;2;3;4;5;6*y"),
            vec![Action::CSI(CSI::Window(Box::new(
                Window::ChecksumRectangularArea {
                    request_id: 1,
                    page_number: 2,
                    top: OneBased::new(3),
                    left: OneBased::new(4),
                    bottom: OneBased::new(5),
                    right: OneBased::new(6),
                }
            )))]
        );
    }

    #[test]
    fn dec_private_modes() {
        assert_eq!(
            parse_as("\x1b[?1;1006h", "\x1b[?1h\x1b[?1006h"),
            vec![
                Action::CSI(CSI::Mode(Mode::SetDecPrivateMode(DecPrivateMode::Code(
                    DecPrivateModeCode::ApplicationCursorKeys
                ),))),
                Action::CSI(CSI::Mode(Mode::SetDecPrivateMode(DecPrivateMode::Code(
                    DecPrivateModeCode::SGRMouse
                ),))),
            ]
        );
    }

    #[test]
    fn xtsmgraphics() {
        assert_eq!(
            round_trip_parse("\x1b[?1;3;256S"),
            vec![Action::CSI(CSI::Device(Box::new(Device::XtSmGraphics(
                XtSmGraphics {
                    item: XtSmGraphicsItem::NumberOfColorRegisters,
                    action_or_status: 3,
                    value: vec![256]
                }
            ))))]
        );
    }

    #[test]
    fn req_attr() {
        assert_eq!(
            round_trip_parse("\x1b[=c"),
            vec![Action::CSI(CSI::Device(Box::new(
                Device::RequestTertiaryDeviceAttributes
            )))]
        );
        assert_eq!(
            round_trip_parse("\x1b[>c"),
            vec![Action::CSI(CSI::Device(Box::new(
                Device::RequestSecondaryDeviceAttributes
            )))]
        );
    }

    #[test]
    fn sgr() {
        assert_eq!(
            parse_as("\x1b[;4m", "\x1b[0m\x1b[4m"),
            vec![
                Action::CSI(CSI::Sgr(Sgr::Reset)),
                Action::CSI(CSI::Sgr(Sgr::Underline(Underline::Single))),
            ]
        );
    }

    #[test]
    fn kitty_img() {
        use crate::apc::*;
        assert_eq!(
            round_trip_parse("\x1b_Gf=24,s=10,v=20;aGVsbG8=\x1b\\"),
            vec![
                Action::KittyImage(Box::new(KittyImage::TransmitData {
                    transmit: KittyImageTransmit {
                        format: Some(KittyImageFormat::Rgb),
                        data: KittyImageData::Direct("aGVsbG8=".to_string()),
                        width: Some(10),
                        height: Some(20),
                        image_id: None,
                        image_number: None,
                        compression: KittyImageCompression::None,
                        more_data_follows: false,
                        alt_text: None,
                    },
                    verbosity: KittyImageVerbosity::Verbose,
                })),
                Action::Esc(Esc::Code(EscCode::StringTerminator)),
            ]
        );

        assert_eq!(
            parse_as(
                "\x1b_Ga=q,s=1,v=1,i=1;YWJjZA==\x1b\\",
                "\x1b_Ga=q,i=1,s=1,v=1;YWJjZA==\x1b\\"
            ),
            vec![
                Action::KittyImage(Box::new(KittyImage::Query {
                    transmit: KittyImageTransmit {
                        format: None,
                        data: KittyImageData::Direct("YWJjZA==".to_string()),
                        width: Some(1),
                        height: Some(1),
                        image_id: Some(1),
                        image_number: None,
                        compression: KittyImageCompression::None,
                        more_data_follows: false,
                        alt_text: None,
                    },
                })),
                Action::Esc(Esc::Code(EscCode::StringTerminator)),
            ]
        );
        assert_eq!(
            parse_as(
                "\x1b_Ga=q,t=f,s=1,v=1,i=2;L3Zhci90bXAvdG1wdGYxd3E4Ym4=\x1b\\",
                "\x1b_Ga=q,i=2,s=1,t=f,v=1;L3Zhci90bXAvdG1wdGYxd3E4Ym4=\x1b\\"
            ),
            vec![
                Action::KittyImage(Box::new(KittyImage::Query {
                    transmit: KittyImageTransmit {
                        format: None,
                        data: KittyImageData::File {
                            path: "/var/tmp/tmptf1wq8bn".to_string(),
                            data_offset: None,
                            data_size: None,
                        },
                        width: Some(1),
                        height: Some(1),
                        image_id: Some(2),
                        image_number: None,
                        compression: KittyImageCompression::None,
                        more_data_follows: false,
                        alt_text: None,
                    },
                })),
                Action::Esc(Esc::Code(EscCode::StringTerminator)),
            ]
        );
    }

    /* Withdrawn because xterm introduced a conflict:
     * <https://github.com/mintty/mintty/issues/1171#issuecomment-1336174469>
     * <https://github.com/mintty/mintty/issues/1189>
    #[test]
    fn dec_private_sgr() {
        use crate::cell::{VerticalAlign};
        assert_eq!(
            parse_as("\x1b[?0m", "\x1b[0m"),
            vec![Action::CSI(CSI::Sgr(Sgr::Reset))]
        );
        assert_eq!(
            parse_as("\x1b[?4m", "\x1b[73m"),
            vec![Action::CSI(CSI::Sgr(Sgr::VerticalAlign(
                VerticalAlign::SuperScript
            )))]
        );
        assert_eq!(
            parse_as("\x1b[?5m", "\x1b[74m"),
            vec![Action::CSI(CSI::Sgr(Sgr::VerticalAlign(
                VerticalAlign::SubScript
            )))]
        );
        assert_eq!(
            parse_as("\x1b[?24m", "\x1b[75m"),
            vec![Action::CSI(CSI::Sgr(Sgr::VerticalAlign(
                VerticalAlign::BaseLine
            )))]
        );
        assert_eq!(
            parse_as("\x1b[?6m", "\x1b[53m"),
            vec![Action::CSI(CSI::Sgr(Sgr::Overline(true)))]
        );
        assert_eq!(
            parse_as("\x1b[?26m", "\x1b[55m"),
            vec![Action::CSI(CSI::Sgr(Sgr::Overline(false)))]
        );
    }
    */

    #[test]
    fn decset() {
        assert_eq!(
            round_trip_parse("\x1b[?23434h"),
            vec![Action::CSI(CSI::Mode(Mode::SetDecPrivateMode(
                DecPrivateMode::Unspecified(23434),
            )))]
        );

        // ft-av13k: DECRQM query for DEC private mode 2026
        // (\x1b[?2026$p). Intermediates are embedded directly in the
        // CsiParam stream as CsiParam::P variants — the older
        // signature with a separate `intermediates: &[u8]` arg was
        // removed when CsiParam absorbed them. The dispatch arm at
        // csi.rs:1852-1855 matches this exact triple.
        {
            let res: Vec<_> = CSI::parse(
                &[
                    CsiParam::P(b'?'),
                    CsiParam::Integer(2026),
                    CsiParam::P(b'$'),
                ],
                false,
                'p',
            )
            .map(Action::CSI)
            .collect();
            assert_eq!(encode(&res), "\x1b[?2026$p");
        }

        // ft-av13k: parse-from-bytes round-trip — feed the actual
        // wire bytes through the VT state machine and confirm we
        // dispatch QueryDecPrivateMode for DEC mode 2026
        // (SynchronizedOutput).
        assert_eq!(
            round_trip_parse("\x1b[?2026$p"),
            vec![Action::CSI(CSI::Mode(Mode::QueryDecPrivateMode(
                DecPrivateMode::Code(DecPrivateModeCode::SynchronizedOutput,)
            )))]
        );

        assert_eq!(
            round_trip_parse("\x1b[?1l"),
            vec![Action::CSI(CSI::Mode(Mode::ResetDecPrivateMode(
                DecPrivateMode::Code(DecPrivateModeCode::ApplicationCursorKeys,)
            )))]
        );

        assert_eq!(
            round_trip_parse("\x1b[?25s"),
            vec![Action::CSI(CSI::Mode(Mode::SaveDecPrivateMode(
                DecPrivateMode::Code(DecPrivateModeCode::ShowCursor,)
            )))]
        );
        assert_eq!(
            round_trip_parse("\x1b[?2004r"),
            vec![Action::CSI(CSI::Mode(Mode::RestoreDecPrivateMode(
                DecPrivateMode::Code(DecPrivateModeCode::BracketedPaste),
            )))]
        );
        assert_eq!(
            round_trip_parse("\x1b[?12h\x1b[?25h"),
            vec![
                Action::CSI(CSI::Mode(Mode::SetDecPrivateMode(DecPrivateMode::Code(
                    DecPrivateModeCode::StartBlinkingCursor,
                )))),
                Action::CSI(CSI::Mode(Mode::SetDecPrivateMode(DecPrivateMode::Code(
                    DecPrivateModeCode::ShowCursor,
                )))),
            ]
        );

        assert_eq!(
            round_trip_parse("\x1b[?1002h\x1b[?1003h\x1b[?1005h\x1b[?1006h"),
            vec![
                Action::CSI(CSI::Mode(Mode::SetDecPrivateMode(DecPrivateMode::Code(
                    DecPrivateModeCode::ButtonEventMouse,
                )))),
                Action::CSI(CSI::Mode(Mode::SetDecPrivateMode(DecPrivateMode::Code(
                    DecPrivateModeCode::AnyEventMouse,
                )))),
                Action::CSI(CSI::Mode(Mode::SetDecPrivateMode(DecPrivateMode::Code(
                    DecPrivateModeCode::Utf8Mouse
                )))),
                Action::CSI(CSI::Mode(Mode::SetDecPrivateMode(DecPrivateMode::Code(
                    DecPrivateModeCode::SGRMouse,
                )))),
            ]
        );
    }

    #[test]
    fn issue_1291() {
        use crate::osc::{ITermDimension, ITermFileData, ITermProprietary};

        let mut p = Parser::new();
        // Note the empty k=v pair immediately following `File=`
        let actions = p.parse_as_vec(b"\x1b]1337;File=;size=234:aGVsbG8=\x07");
        assert_eq!(
            vec![Action::OperatingSystemCommand(Box::new(
                OperatingSystemCommand::ITermProprietary(ITermProprietary::File(Box::new(
                    ITermFileData {
                        name: None,
                        size: Some(234),
                        width: ITermDimension::Automatic,
                        height: ITermDimension::Automatic,
                        preserve_aspect_ratio: true,
                        inline: false,
                        do_not_move_cursor: false,
                        data: b"hello".to_vec(),
                    }
                )))
            ))],
            actions
        );
    }

    #[test]
    fn itermfiledata_oob() {
        let mut p = Parser::new();
        p.parse_as_vec(b"\x9d1337\xff;File\x1b");
    }

    /// vtparse's MAX_OSC was set too low to fully parse this escape sequence.
    /// This test verifies that the correct number of actions comes back.
    #[test]
    fn dynamic_colors() {
        let mut p = Parser::new();
        let actions = p.parse_as_vec(b"\x1b]4;0;#000000;1;#aa3731;2;#448c27;3;#cb9000;4;#325cc0;5;#7a3e9d;6;#0083b2;7;#f7f7f7;8;#777777;9;#f05050;10;#60cb00;11;#ffbc5d;12;#007acc;13;#e64ce6;14;#00aacb;15;#f7f7f7\x07");
        k9::snapshot!(
            actions,
            "
[
    OperatingSystemCommand(
        ChangeColorNumber(
            [
                ChangeColorPair {
                    palette_index: 0,
                    color: Color(
                        SrgbaTuple(
                            0.0,
                            0.0,
                            0.0,
                            1.0,
                        ),
                    ),
                },
                ChangeColorPair {
                    palette_index: 1,
                    color: Color(
                        SrgbaTuple(
                            0.6666667,
                            0.21568628,
                            0.19215687,
                            1.0,
                        ),
                    ),
                },
                ChangeColorPair {
                    palette_index: 2,
                    color: Color(
                        SrgbaTuple(
                            0.26666668,
                            0.54901963,
                            0.15294118,
                            1.0,
                        ),
                    ),
                },
                ChangeColorPair {
                    palette_index: 3,
                    color: Color(
                        SrgbaTuple(
                            0.79607844,
                            0.5647059,
                            0.0,
                            1.0,
                        ),
                    ),
                },
                ChangeColorPair {
                    palette_index: 4,
                    color: Color(
                        SrgbaTuple(
                            0.19607843,
                            0.36078432,
                            0.7529412,
                            1.0,
                        ),
                    ),
                },
                ChangeColorPair {
                    palette_index: 5,
                    color: Color(
                        SrgbaTuple(
                            0.47843137,
                            0.24313726,
                            0.6156863,
                            1.0,
                        ),
                    ),
                },
                ChangeColorPair {
                    palette_index: 6,
                    color: Color(
                        SrgbaTuple(
                            0.0,
                            0.5137255,
                            0.69803923,
                            1.0,
                        ),
                    ),
                },
                ChangeColorPair {
                    palette_index: 7,
                    color: Color(
                        SrgbaTuple(
                            0.96862745,
                            0.96862745,
                            0.96862745,
                            1.0,
                        ),
                    ),
                },
                ChangeColorPair {
                    palette_index: 8,
                    color: Color(
                        SrgbaTuple(
                            0.46666667,
                            0.46666667,
                            0.46666667,
                            1.0,
                        ),
                    ),
                },
                ChangeColorPair {
                    palette_index: 9,
                    color: Color(
                        SrgbaTuple(
                            0.9411765,
                            0.3137255,
                            0.3137255,
                            1.0,
                        ),
                    ),
                },
                ChangeColorPair {
                    palette_index: 10,
                    color: Color(
                        SrgbaTuple(
                            0.3764706,
                            0.79607844,
                            0.0,
                            1.0,
                        ),
                    ),
                },
                ChangeColorPair {
                    palette_index: 11,
                    color: Color(
                        SrgbaTuple(
                            1.0,
                            0.7372549,
                            0.3647059,
                            1.0,
                        ),
                    ),
                },
                ChangeColorPair {
                    palette_index: 12,
                    color: Color(
                        SrgbaTuple(
                            0.0,
                            0.47843137,
                            0.8,
                            1.0,
                        ),
                    ),
                },
                ChangeColorPair {
                    palette_index: 13,
                    color: Color(
                        SrgbaTuple(
                            0.9019608,
                            0.29803923,
                            0.9019608,
                            1.0,
                        ),
                    ),
                },
                ChangeColorPair {
                    palette_index: 14,
                    color: Color(
                        SrgbaTuple(
                            0.0,
                            0.6666667,
                            0.79607844,
                            1.0,
                        ),
                    ),
                },
                ChangeColorPair {
                    palette_index: 15,
                    color: Color(
                        SrgbaTuple(
                            0.96862745,
                            0.96862745,
                            0.96862745,
                            1.0,
                        ),
                    ),
                },
            ],
        ),
    ),
]
"
        );
    }
}
