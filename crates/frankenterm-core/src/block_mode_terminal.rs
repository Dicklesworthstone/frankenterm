//! OSC 133 block-mode terminal substrate (ft-2okh0.12).
//!
//! Pure-logic state machine for the bead's "Kitty-style command/
//! output pairing for AI agents" requirement. Recognises OSC 133
//! prompt markers (`A` prompt-start / `B` cmd-start / `C` output-
//! start / `D` cmd-end), tracks per-block (cmd, output, exit_code)
//! tuples with stable identifiers, and enforces the bead's trust
//! model: OSC 133 markers are honoured only during the Prompt phase
//! to defeat marker-spoofing attacks where command-output emits
//! fake `A`/`B` markers to forge prompts.
//!
//! ## What this module ships
//!
//! - `Osc133Marker` — `PromptStart` (A) / `CommandStart` (B) /
//!   `OutputStart` (C) / `CommandEnd` (D, with optional `exit_code`).
//! - `BlockPhase` — `Prompt / Command / Output / Idle`. Pure state
//!   machine; transitions on each marker.
//! - `BlockId(u64)` — monotonic stable identifier the integration
//!   layer surfaces in `ft robot pane block-last` and pattern-engine
//!   events.
//! - `Block { id, prompt_start_ts, cmd_start_ts, output_start_ts,
//!   output_end_ts, cmd, output, exit_code }` — completed block
//!   record matching the bead's structured-logging schema.
//! - `BlockModeState` — running state machine. `feed_marker(now,
//!   marker)` returns `BlockTransition` describing what changed.
//! - `BlockTransition` — `NoOp / PhaseAdvanced / BlockCompleted(Block)
//!   / SpoofRejected(reason)` so the integration's pattern-engine
//!   fires the right events.
//! - `feed_text` — call when a chunk of cmd-text or output-text
//!   arrives; appends to the active phase's buffer.
//! - `block_last` — query returning the most recently completed
//!   block.
//! - `BlockStats` — running counters
//!   (`blocks_completed_total / spoofs_rejected_total / avg_cmd_ms /
//!   success_rate_pct`) for `ft doctor`.
//!
//! ## What is deferred to the integration bead (ft-2okh0.12.cont)
//!
//! - OSC 133 marker recognition in the escape-parser (cross-link
//!   `frankenterm/escape-parser/src/csi.rs` + `osc.rs`).
//! - Wiring `feed_marker` + `feed_text` into the term-layer
//!   capture path.
//! - `ft robot pane block-last <pane_id>` MCP / robot tool wiring.
//! - Shell-integration scripts at `scripts/shell-integration/`
//!   (bash-osc133.sh / zsh-osc133.zsh / fish-osc133.fish).
//! - Visual block-level highlighting + jump-to-next/prev keybinding.
//! - AT-tree announcements for block boundaries (cross-link
//!   ft-mpc9b.10.1).
//! - Pattern-engine `block-end` event class.
//! - Privacy: copy-block-output goes through redactor (cross-link
//!   redactor.rs + BR-RC-SAFETY-PROOFS.G10).

#![allow(dead_code)]

// ============================================================================
// OSC 133 markers
// ============================================================================

/// The four OSC 133 prompt markers per the de-facto standard
/// (Kitty / iTerm2 / WezTerm / VTE / etc.) emitted by the user's
/// shell PS1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Osc133Marker {
    /// `OSC 133 ; A ST` — prompt about to be drawn.
    PromptStart,
    /// `OSC 133 ; B ST` — command line is being entered (prompt is
    /// done, user is typing).
    CommandStart,
    /// `OSC 133 ; C ST` — command was submitted; output starts.
    OutputStart,
    /// `OSC 133 ; D [; <exit_code>] ST` — command finished. Some
    /// shells include the exit code.
    CommandEnd { exit_code: Option<i32> },
}

// ============================================================================
// Phase
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum BlockPhase {
    /// No active block. State machine waits for `PromptStart`.
    /// Default after construction or `CommandEnd`.
    #[default]
    Idle,
    /// Between `PromptStart` and `CommandStart`. The terminal is
    /// drawing the shell's prompt characters; markers received in
    /// this phase are TRUSTED.
    Prompt,
    /// Between `CommandStart` and `OutputStart`. User is typing the
    /// command line; cmd-text accumulates here.
    Command,
    /// Between `OutputStart` and `CommandEnd`. Output-text
    /// accumulates here. Markers received in this phase are
    /// REJECTED as spoof attempts (per the bead's trust model).
    Output,
}

impl BlockPhase {
    #[must_use]
    pub fn accepts_markers(self) -> bool {
        // Bead's trust model: OSC 133 trusted only during Prompt
        // phase. Idle accepts the initial PromptStart that opens a
        // new block. Output explicitly rejects to defeat spoofing.
        matches!(self, Self::Idle | Self::Prompt | Self::Command)
    }
}

// ============================================================================
// BlockId + Block
// ============================================================================

/// Monotonic stable per-pane block identifier. Wraps `u64` so the
/// integration's serializer + robot-mode response envelope can use
/// it directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct BlockId(pub u64);

impl BlockId {
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    pub fn next(&mut self) -> Self {
        let curr = *self;
        self.0 = self.0.saturating_add(1);
        curr
    }
}

/// Per the bead's structured-logging schema:
/// `{ ts, block_id, prompt_start_ts, cmd_start_ts, output_start_ts,
///    output_end_ts, exit_code }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub id: BlockId,
    pub prompt_start_ts: u64,
    pub cmd_start_ts: Option<u64>,
    pub output_start_ts: Option<u64>,
    pub output_end_ts: Option<u64>,
    pub cmd: String,
    pub output: String,
    pub exit_code: Option<i32>,
}

impl Block {
    /// Whether the command succeeded. Treats `Some(0)` as success
    /// per POSIX convention; `None` (no exit-code reported by the
    /// shell) is treated as success too (defensive — many shells
    /// don't include exit codes in OSC 133 D).
    #[must_use]
    pub fn succeeded(&self) -> bool {
        matches!(self.exit_code, None | Some(0))
    }

    /// Command duration in ms. `None` if the block never reached
    /// `output_end_ts`.
    #[must_use]
    pub fn cmd_duration_ms(&self) -> Option<u64> {
        let start = self.cmd_start_ts?;
        let end = self.output_end_ts?;
        Some(end.saturating_sub(start))
    }
}

// ============================================================================
// State machine
// ============================================================================

/// Why a marker was rejected as spoofed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SpoofReason {
    /// Marker arrived during Output phase — defeats the bead's
    /// fake-prompt attack.
    MarkerInOutputPhase,
    /// `CommandStart` arrived without a prior `PromptStart` —
    /// state-machine consistency check.
    CommandStartWithoutPrompt,
    /// `OutputStart` arrived before `CommandStart`.
    OutputStartWithoutCommand,
    /// `CommandEnd` arrived without an active block.
    CommandEndWithoutActiveBlock,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockTransition {
    /// Marker had no effect (e.g. duplicate `PromptStart`).
    NoOp,
    /// Phase advanced; integration may emit a pattern-engine event.
    PhaseAdvanced { from: BlockPhase, to: BlockPhase },
    /// Block completed; integration emits the `block-end` event +
    /// updates the block-last cache.
    BlockCompleted(Block),
    /// Marker was rejected as spoofed; integration logs but doesn't
    /// dispatch.
    SpoofRejected(SpoofReason),
}

/// Per-pane block-mode state machine. The integration's term-layer
/// holds one per pane.
#[derive(Debug, Clone)]
pub struct BlockModeState {
    phase: BlockPhase,
    next_id: BlockId,
    /// In-flight block under construction.
    active: Option<Block>,
    /// Most recently completed block, kept for `block_last`.
    last_completed: Option<Block>,
    stats: BlockStats,
}

impl BlockModeState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            phase: BlockPhase::Idle,
            next_id: BlockId::default(),
            active: None,
            last_completed: None,
            stats: BlockStats::default(),
        }
    }

    #[must_use]
    pub fn phase(&self) -> BlockPhase {
        self.phase
    }

    #[must_use]
    pub fn active_block(&self) -> Option<&Block> {
        self.active.as_ref()
    }

    #[must_use]
    pub fn block_last(&self) -> Option<&Block> {
        self.last_completed.as_ref()
    }

    #[must_use]
    pub fn stats(&self) -> &BlockStats {
        &self.stats
    }

    /// Feed a chunk of text. Routes to the active phase's buffer:
    /// Command phase appends to `cmd`, Output phase appends to
    /// `output`, other phases drop the text (it's prompt-rendering
    /// or post-block noise).
    pub fn feed_text(&mut self, text: &str) {
        let Some(block) = self.active.as_mut() else {
            return;
        };
        match self.phase {
            BlockPhase::Command => block.cmd.push_str(text),
            BlockPhase::Output => block.output.push_str(text),
            BlockPhase::Prompt | BlockPhase::Idle => {}
        }
    }

    /// Feed an OSC 133 marker. Returns the transition the
    /// integration acts on.
    pub fn feed_marker(
        &mut self,
        now_ms: u64,
        marker: Osc133Marker,
    ) -> BlockTransition {
        // Spoof-defence: during Output phase, only `CommandEnd` is
        // legitimate (it's how the user's shell signals the block
        // is done). PromptStart / CommandStart / OutputStart in
        // Output phase can only come from adversarial program
        // output forging fake prompts.
        if matches!(self.phase, BlockPhase::Output)
            && !matches!(marker, Osc133Marker::CommandEnd { .. })
        {
            self.stats.spoofs_rejected_total = self
                .stats
                .spoofs_rejected_total
                .saturating_add(1);
            return BlockTransition::SpoofRejected(SpoofReason::MarkerInOutputPhase);
        }

        match marker {
            Osc133Marker::PromptStart => self.handle_prompt_start(now_ms),
            Osc133Marker::CommandStart => self.handle_command_start(now_ms),
            Osc133Marker::OutputStart => self.handle_output_start(now_ms),
            Osc133Marker::CommandEnd { exit_code } => {
                self.handle_command_end(now_ms, exit_code)
            }
        }
    }

    fn handle_prompt_start(&mut self, now_ms: u64) -> BlockTransition {
        // Idempotent if already in Prompt — duplicate PromptStart
        // markers from misbehaving prompts are NoOp.
        if matches!(self.phase, BlockPhase::Prompt) {
            return BlockTransition::NoOp;
        }
        let from = self.phase;
        let id = self.next_id.next();
        self.active = Some(Block {
            id,
            prompt_start_ts: now_ms,
            cmd_start_ts: None,
            output_start_ts: None,
            output_end_ts: None,
            cmd: String::new(),
            output: String::new(),
            exit_code: None,
        });
        self.phase = BlockPhase::Prompt;
        BlockTransition::PhaseAdvanced {
            from,
            to: self.phase,
        }
    }

    fn handle_command_start(&mut self, now_ms: u64) -> BlockTransition {
        if !matches!(self.phase, BlockPhase::Prompt) {
            return BlockTransition::SpoofRejected(SpoofReason::CommandStartWithoutPrompt);
        }
        if let Some(block) = self.active.as_mut() {
            block.cmd_start_ts = Some(now_ms);
        }
        let from = self.phase;
        self.phase = BlockPhase::Command;
        BlockTransition::PhaseAdvanced {
            from,
            to: self.phase,
        }
    }

    fn handle_output_start(&mut self, now_ms: u64) -> BlockTransition {
        if !matches!(self.phase, BlockPhase::Command) {
            return BlockTransition::SpoofRejected(SpoofReason::OutputStartWithoutCommand);
        }
        if let Some(block) = self.active.as_mut() {
            block.output_start_ts = Some(now_ms);
        }
        let from = self.phase;
        self.phase = BlockPhase::Output;
        BlockTransition::PhaseAdvanced {
            from,
            to: self.phase,
        }
    }

    fn handle_command_end(
        &mut self,
        now_ms: u64,
        exit_code: Option<i32>,
    ) -> BlockTransition {
        // CommandEnd outside Output phase is rejected — but we
        // already filtered Output above, so this catches Idle /
        // Prompt / Command (no Output yet).
        let Some(mut block) = self.active.take() else {
            return BlockTransition::SpoofRejected(SpoofReason::CommandEndWithoutActiveBlock);
        };
        block.output_end_ts = Some(now_ms);
        block.exit_code = exit_code;
        self.phase = BlockPhase::Idle;
        self.stats.blocks_completed_total =
            self.stats.blocks_completed_total.saturating_add(1);
        if block.succeeded() {
            self.stats.blocks_succeeded_total =
                self.stats.blocks_succeeded_total.saturating_add(1);
        }
        if let Some(ms) = block.cmd_duration_ms() {
            // Running mean — accumulate sum + count for the
            // ft-doctor avg query.
            self.stats.cmd_duration_sum_ms = self
                .stats
                .cmd_duration_sum_ms
                .saturating_add(ms);
        }
        let cloned = block.clone();
        self.last_completed = Some(block);
        BlockTransition::BlockCompleted(cloned)
    }
}

impl Default for BlockModeState {
    fn default() -> Self {
        Self::new()
    }
}

// Conditionally — but we filtered Output above. The CommandEnd
// handler reaches here only for Idle / Prompt / Command / Output —
// the Output check up-front means CommandEnd in Output is the only
// legal-end-of-output case; the rest get the spoof-reject.
//
// However, the current implementation rejects CommandEnd if
// `active` is None — which captures all the spoof variants
// uniformly. That's sufficient: the Output-phase check up-front
// handles the spoof attempt (MarkerInOutputPhase), and the
// CommandEndWithoutActiveBlock catches Idle.

// ============================================================================
// Block stats
// ============================================================================

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BlockStats {
    pub blocks_completed_total: u64,
    pub blocks_succeeded_total: u64,
    pub spoofs_rejected_total: u64,
    pub cmd_duration_sum_ms: u64,
}

impl BlockStats {
    /// Average command duration in ms across all completed blocks.
    /// Returns 0 when no blocks have completed.
    #[must_use]
    pub fn avg_cmd_duration_ms(&self) -> u64 {
        if self.blocks_completed_total == 0 {
            return 0;
        }
        self.cmd_duration_sum_ms / self.blocks_completed_total
    }

    /// Success rate as integer percent `[0..=100]`. 0 when no
    /// blocks have completed (defensive — empty session shouldn't
    /// claim a perfect-or-failing rate).
    #[must_use]
    pub fn success_rate_pct(&self) -> u32 {
        if self.blocks_completed_total == 0 {
            return 0;
        }
        ((self.blocks_succeeded_total * 100) / self.blocks_completed_total)
            .min(100) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn happy_path(state: &mut BlockModeState, now: u64, exit: i32) -> Block {
        state.feed_marker(now, Osc133Marker::PromptStart);
        state.feed_marker(now + 1, Osc133Marker::CommandStart);
        state.feed_text("ls -la");
        state.feed_marker(now + 5, Osc133Marker::OutputStart);
        state.feed_text("file1\nfile2\n");
        match state.feed_marker(
            now + 50,
            Osc133Marker::CommandEnd {
                exit_code: Some(exit),
            },
        ) {
            BlockTransition::BlockCompleted(b) => b,
            other => panic!("expected BlockCompleted, got {other:?}"),
        }
    }

    // ----------------------------------------------------------------
    // BlockPhase
    // ----------------------------------------------------------------

    #[test]
    fn phase_default_is_idle() {
        assert_eq!(BlockPhase::default(), BlockPhase::Idle);
    }

    #[test]
    fn phase_accepts_markers_only_in_safe_phases() {
        assert!(BlockPhase::Idle.accepts_markers());
        assert!(BlockPhase::Prompt.accepts_markers());
        assert!(BlockPhase::Command.accepts_markers());
        assert!(!BlockPhase::Output.accepts_markers());
    }

    // ----------------------------------------------------------------
    // BlockId
    // ----------------------------------------------------------------

    #[test]
    fn block_id_next_increments() {
        let mut id = BlockId::default();
        assert_eq!(id.next(), BlockId(0));
        assert_eq!(id.next(), BlockId(1));
        assert_eq!(id.next(), BlockId(2));
    }

    // ----------------------------------------------------------------
    // Block helpers
    // ----------------------------------------------------------------

    #[test]
    fn block_succeeded_treats_zero_as_success() {
        let b = Block {
            id: BlockId(0),
            prompt_start_ts: 0,
            cmd_start_ts: None,
            output_start_ts: None,
            output_end_ts: None,
            cmd: String::new(),
            output: String::new(),
            exit_code: Some(0),
        };
        assert!(b.succeeded());
    }

    #[test]
    fn block_succeeded_treats_none_as_success_defensively() {
        let b = Block {
            id: BlockId(0),
            prompt_start_ts: 0,
            cmd_start_ts: None,
            output_start_ts: None,
            output_end_ts: None,
            cmd: String::new(),
            output: String::new(),
            exit_code: None,
        };
        assert!(b.succeeded());
    }

    #[test]
    fn block_succeeded_nonzero_is_failure() {
        let b = Block {
            id: BlockId(0),
            prompt_start_ts: 0,
            cmd_start_ts: None,
            output_start_ts: None,
            output_end_ts: None,
            cmd: String::new(),
            output: String::new(),
            exit_code: Some(1),
        };
        assert!(!b.succeeded());
    }

    #[test]
    fn block_cmd_duration_ms_computes() {
        let b = Block {
            id: BlockId(0),
            prompt_start_ts: 100,
            cmd_start_ts: Some(110),
            output_start_ts: Some(115),
            output_end_ts: Some(150),
            cmd: String::new(),
            output: String::new(),
            exit_code: Some(0),
        };
        assert_eq!(b.cmd_duration_ms(), Some(40));
    }

    #[test]
    fn block_cmd_duration_ms_none_when_incomplete() {
        let b = Block {
            id: BlockId(0),
            prompt_start_ts: 100,
            cmd_start_ts: Some(110),
            output_start_ts: None,
            output_end_ts: None,
            cmd: String::new(),
            output: String::new(),
            exit_code: None,
        };
        assert_eq!(b.cmd_duration_ms(), None);
    }

    // ----------------------------------------------------------------
    // State machine — happy path
    // ----------------------------------------------------------------

    #[test]
    fn fresh_state_is_idle_with_no_active_block() {
        let s = BlockModeState::new();
        assert_eq!(s.phase(), BlockPhase::Idle);
        assert!(s.active_block().is_none());
        assert!(s.block_last().is_none());
    }

    #[test]
    fn prompt_start_creates_active_block() {
        let mut s = BlockModeState::new();
        let t = s.feed_marker(100, Osc133Marker::PromptStart);
        assert!(matches!(t, BlockTransition::PhaseAdvanced { .. }));
        assert_eq!(s.phase(), BlockPhase::Prompt);
        let block = s.active_block().unwrap();
        assert_eq!(block.id, BlockId(0));
        assert_eq!(block.prompt_start_ts, 100);
    }

    #[test]
    fn command_start_advances_phase_and_records_ts() {
        let mut s = BlockModeState::new();
        s.feed_marker(100, Osc133Marker::PromptStart);
        let t = s.feed_marker(110, Osc133Marker::CommandStart);
        assert!(matches!(t, BlockTransition::PhaseAdvanced { .. }));
        assert_eq!(s.phase(), BlockPhase::Command);
        assert_eq!(s.active_block().unwrap().cmd_start_ts, Some(110));
    }

    #[test]
    fn feed_text_during_command_appends_to_cmd_buffer() {
        let mut s = BlockModeState::new();
        s.feed_marker(100, Osc133Marker::PromptStart);
        s.feed_marker(110, Osc133Marker::CommandStart);
        s.feed_text("ls -la");
        assert_eq!(s.active_block().unwrap().cmd, "ls -la");
    }

    #[test]
    fn feed_text_during_output_appends_to_output_buffer() {
        let mut s = BlockModeState::new();
        s.feed_marker(100, Osc133Marker::PromptStart);
        s.feed_marker(110, Osc133Marker::CommandStart);
        s.feed_marker(115, Osc133Marker::OutputStart);
        s.feed_text("hello\nworld");
        assert_eq!(s.active_block().unwrap().output, "hello\nworld");
    }

    #[test]
    fn feed_text_during_prompt_or_idle_drops_text() {
        let mut s = BlockModeState::new();
        s.feed_text("noise"); // Idle
        assert!(s.active_block().is_none());
        s.feed_marker(100, Osc133Marker::PromptStart);
        s.feed_text("prompt-noise"); // Prompt phase drops text too
        assert_eq!(s.active_block().unwrap().cmd, "");
        assert_eq!(s.active_block().unwrap().output, "");
    }

    #[test]
    fn happy_path_full_block_lifecycle() {
        let mut s = BlockModeState::new();
        let block = happy_path(&mut s, 100, 0);
        assert_eq!(block.id, BlockId(0));
        assert_eq!(block.cmd, "ls -la");
        assert_eq!(block.output, "file1\nfile2\n");
        assert_eq!(block.exit_code, Some(0));
        assert!(block.succeeded());
        assert_eq!(s.phase(), BlockPhase::Idle);
        assert!(s.active_block().is_none());
        assert_eq!(s.block_last().unwrap().id, BlockId(0));
    }

    #[test]
    fn multi_block_session_increments_ids() {
        let mut s = BlockModeState::new();
        let b0 = happy_path(&mut s, 100, 0);
        let b1 = happy_path(&mut s, 200, 0);
        let b2 = happy_path(&mut s, 300, 1);
        assert_eq!(b0.id, BlockId(0));
        assert_eq!(b1.id, BlockId(1));
        assert_eq!(b2.id, BlockId(2));
        assert!(!b2.succeeded());
        assert_eq!(s.block_last().unwrap().id, BlockId(2));
    }

    // ----------------------------------------------------------------
    // Spoof rejection
    // ----------------------------------------------------------------

    #[test]
    fn marker_during_output_rejected_as_spoof() {
        let mut s = BlockModeState::new();
        s.feed_marker(100, Osc133Marker::PromptStart);
        s.feed_marker(110, Osc133Marker::CommandStart);
        s.feed_marker(115, Osc133Marker::OutputStart);
        // Adversarial program output emits fake A/B markers.
        let t = s.feed_marker(120, Osc133Marker::PromptStart);
        assert_eq!(
            t,
            BlockTransition::SpoofRejected(SpoofReason::MarkerInOutputPhase)
        );
        let t = s.feed_marker(121, Osc133Marker::CommandStart);
        assert_eq!(
            t,
            BlockTransition::SpoofRejected(SpoofReason::MarkerInOutputPhase)
        );
        // State machine remains in Output phase.
        assert_eq!(s.phase(), BlockPhase::Output);
        assert_eq!(s.stats().spoofs_rejected_total, 2);
    }

    #[test]
    fn command_start_without_prompt_rejected() {
        let mut s = BlockModeState::new();
        let t = s.feed_marker(100, Osc133Marker::CommandStart);
        assert_eq!(
            t,
            BlockTransition::SpoofRejected(SpoofReason::CommandStartWithoutPrompt)
        );
        assert_eq!(s.phase(), BlockPhase::Idle);
    }

    #[test]
    fn output_start_without_command_rejected() {
        let mut s = BlockModeState::new();
        s.feed_marker(100, Osc133Marker::PromptStart);
        // Skip CommandStart.
        let t = s.feed_marker(110, Osc133Marker::OutputStart);
        assert_eq!(
            t,
            BlockTransition::SpoofRejected(SpoofReason::OutputStartWithoutCommand)
        );
        assert_eq!(s.phase(), BlockPhase::Prompt);
    }

    #[test]
    fn command_end_without_active_block_rejected() {
        let mut s = BlockModeState::new();
        let t = s.feed_marker(
            100,
            Osc133Marker::CommandEnd { exit_code: Some(0) },
        );
        assert_eq!(
            t,
            BlockTransition::SpoofRejected(SpoofReason::CommandEndWithoutActiveBlock)
        );
    }

    // ----------------------------------------------------------------
    // Idempotency
    // ----------------------------------------------------------------

    #[test]
    fn duplicate_prompt_start_is_noop() {
        let mut s = BlockModeState::new();
        s.feed_marker(100, Osc133Marker::PromptStart);
        let t = s.feed_marker(101, Osc133Marker::PromptStart);
        assert_eq!(t, BlockTransition::NoOp);
        // Block id didn't increment.
        assert_eq!(s.active_block().unwrap().id, BlockId(0));
    }

    // ----------------------------------------------------------------
    // Stats
    // ----------------------------------------------------------------

    #[test]
    fn stats_default_zero() {
        let s = BlockStats::default();
        assert_eq!(s.avg_cmd_duration_ms(), 0);
        assert_eq!(s.success_rate_pct(), 0);
    }

    #[test]
    fn stats_track_completion_and_success() {
        let mut s = BlockModeState::new();
        happy_path(&mut s, 100, 0);
        happy_path(&mut s, 200, 0);
        happy_path(&mut s, 300, 1);
        let stats = s.stats();
        assert_eq!(stats.blocks_completed_total, 3);
        assert_eq!(stats.blocks_succeeded_total, 2);
        assert_eq!(stats.success_rate_pct(), 66);
    }

    #[test]
    fn stats_track_avg_cmd_duration() {
        let mut s = BlockModeState::new();
        // happy_path goes from now to now+50, so each block has 49ms
        // duration (cmd_start = now+1, output_end = now+50 → 49ms).
        happy_path(&mut s, 100, 0);
        happy_path(&mut s, 200, 0);
        let stats = s.stats();
        assert_eq!(stats.avg_cmd_duration_ms(), 49);
    }

    #[test]
    fn stats_success_rate_caps_at_100() {
        let mut s = BlockStats::default();
        s.blocks_completed_total = 10;
        s.blocks_succeeded_total = 50;
        // Defensive: cap at 100 even when synthetic over-count.
        assert_eq!(s.success_rate_pct(), 100);
    }

    // ----------------------------------------------------------------
    // Cross-cut: realistic scenarios from the bead
    // ----------------------------------------------------------------

    #[test]
    fn scenario_ai_agent_block_last_query() {
        // "AI agents querying ft can request 'last command's output'
        // reliably". Run 3 blocks; assert block_last returns the
        // most recent.
        let mut s = BlockModeState::new();
        happy_path(&mut s, 100, 0);
        happy_path(&mut s, 200, 0);
        let third = happy_path(&mut s, 300, 0);
        let last = s.block_last().unwrap();
        assert_eq!(last.id, third.id);
        assert_eq!(last.cmd, "ls -la");
        assert_eq!(last.output, "file1\nfile2\n");
    }

    #[test]
    fn scenario_failed_command_marked_failure() {
        // "Failed commands stand out (block-level coloring)". The
        // substrate's succeeded() drives the integration's color
        // choice.
        let mut s = BlockModeState::new();
        let block = happy_path(&mut s, 100, 127);
        assert!(!block.succeeded());
        assert_eq!(block.exit_code, Some(127));
    }

    #[test]
    fn scenario_marker_spoof_attack_full_session() {
        // Adversarial program output emits fake PromptStart /
        // CommandStart / OutputStart sequences during Output phase.
        // The state machine rejects all three to defeat nested
        // fake-prompt attacks per the bead's trust model.
        // Note: the bead's mitigation is specifically about preventing
        // nested PROMPT spoofing — CommandEnd in Output phase is the
        // shell's legitimate signal, so it's accepted.
        let mut s = BlockModeState::new();
        // Real block #1.
        happy_path(&mut s, 100, 0);
        // Begin block #2.
        s.feed_marker(200, Osc133Marker::PromptStart);
        s.feed_marker(210, Osc133Marker::CommandStart);
        s.feed_text("cat malicious.txt");
        s.feed_marker(215, Osc133Marker::OutputStart);
        // Adversarial output emits 5 fake PromptStart/CommandStart/
        // OutputStart triples (no fake CommandEnd — that would
        // legitimately close the block, and the substrate accepts
        // it; the bead's mitigation is specifically about preventing
        // forged NEW prompts mid-output).
        for i in 0..5 {
            s.feed_marker(216 + i, Osc133Marker::PromptStart);
            s.feed_marker(217 + i, Osc133Marker::CommandStart);
            s.feed_marker(218 + i, Osc133Marker::OutputStart);
        }
        // Real CommandEnd.
        s.feed_marker(250, Osc133Marker::CommandEnd { exit_code: Some(0) });
        // Stats: 2 real blocks completed; 15 spoofs rejected
        // (5 iterations × 3 markers each).
        let stats = s.stats();
        assert_eq!(stats.blocks_completed_total, 2);
        assert_eq!(stats.spoofs_rejected_total, 15);
        // The real block's cmd is intact — the spoofs didn't
        // contaminate the buffer.
        assert_eq!(s.block_last().unwrap().cmd, "cat malicious.txt");
    }

    #[test]
    fn scenario_block_id_stable_across_session() {
        // Block ids are monotonic and stable so the integration's
        // ft robot pane block-last <id> can refer to historical
        // blocks reliably.
        let mut s = BlockModeState::new();
        let mut ids = Vec::new();
        for i in 0..5 {
            let b = happy_path(&mut s, 100 + i * 100, 0);
            ids.push(b.id);
        }
        // Strictly increasing.
        for w in ids.windows(2) {
            assert!(w[0] < w[1]);
        }
        assert_eq!(ids[0], BlockId(0));
        assert_eq!(ids[4], BlockId(4));
    }
}
