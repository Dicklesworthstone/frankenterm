//! Kitty keyboard progressive enhancement protocol
//! ([BR-TERM-EMULATOR-UPLIFT-2.1.4] / `ft-2okh0.1.4`).
//!
//! Modern terminals support progressive keyboard-enhancement
//! modes that disambiguate keys (Tab vs Ctrl-I, left-Ctrl vs
//! right-Ctrl, press vs release events). Apps that benefit:
//! nvim with super-key / hyper-key bindings, emacs, helix.
//!
//! ## What this module ships
//!
//! - [`KittyKbdFlag`] — closed list of 5 enhancement flags
//!   (1 / 2 / 4 / 8 / 16). Bitwise composable.
//! - [`KittyKbdFlagSet`] — bitset over the flag space.
//! - [`KittyKbdStack`] — stack-based mode tracking
//!   (push/pop/query) with `MAX_STACK_DEPTH = 16` overflow
//!   protection.
//! - [`KittyKbdCsi`] — closed list of CSI sequences:
//!   `Push`, `Pop`, `Query`.
//! - [`parse_csi_kbd`] — pure parser for `CSI > <flags> u`,
//!   `CSI < u`, `CSI ? u`.
//! - [`render_query_response`] — `CSI ? <flags> u` builder.
//! - [`encode_key_event`] — per-mode encoding of a key
//!   event into terminal bytes.
//! - [`KittyKbdHealth`] — `ft doctor` snapshot mirroring
//!   the bead's "kbd_mode_pushes / kbd_mode_pops /
//!   current_max_depth / kbd_events_by_mode" indicators.
//!
//! ## Stack semantics
//!
//! - Default state: empty stack, behaves as flag-set 0
//!   (legacy mode — every Ctrl-I emits Tab, etc).
//! - `Push(flags)` pushes a fresh frame onto the stack.
//!   Nested apps push/pop without affecting outer scope.
//! - `Pop` removes the top frame. Pop on empty stack is a
//!   no-op (per Kitty protocol — apps must not pop into
//!   negative state).
//! - `Query` does not mutate; just reads the current top.
//! - At `MAX_STACK_DEPTH = 16`, further pushes are
//!   rejected (`PushOutcome::Rejected`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// ============================================================================
// Flag taxonomy
// ============================================================================

/// One enhancement flag — closed list of 5 bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KittyKbdFlag {
    /// `1` — Disambiguate escape codes (Tab vs Ctrl-I).
    Disambiguate,
    /// `2` — Report event types (press/release/repeat).
    ReportEventTypes,
    /// `4` — Report alternate keys (lowercase/uppercase).
    ReportAlternateKeys,
    /// `8` — Report all keys as escape codes.
    ReportAllKeysAsEscapes,
    /// `16` — Report associated text (for IME).
    ReportAssociatedText,
}

impl KittyKbdFlag {
    /// Bit value per the protocol.
    #[must_use]
    pub const fn bit(self) -> u8 {
        match self {
            Self::Disambiguate => 1,
            Self::ReportEventTypes => 2,
            Self::ReportAlternateKeys => 4,
            Self::ReportAllKeysAsEscapes => 8,
            Self::ReportAssociatedText => 16,
        }
    }

    #[must_use]
    pub const fn all() -> [Self; 5] {
        [
            Self::Disambiguate,
            Self::ReportEventTypes,
            Self::ReportAlternateKeys,
            Self::ReportAllKeysAsEscapes,
            Self::ReportAssociatedText,
        ]
    }
}

/// Bitset over the five enhancement flags.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct KittyKbdFlagSet {
    /// Raw bits — only low 5 bits ever set.
    pub bits: u8,
}

impl KittyKbdFlagSet {
    pub const ALL_BITS: u8 = 0b0001_1111;

    #[must_use]
    pub const fn empty() -> Self {
        Self { bits: 0 }
    }

    #[must_use]
    pub const fn from_bits_truncate(bits: u8) -> Self {
        Self {
            bits: bits & Self::ALL_BITS,
        }
    }

    pub fn set(&mut self, flag: KittyKbdFlag) {
        self.bits |= flag.bit();
    }

    #[must_use]
    pub fn contains(self, flag: KittyKbdFlag) -> bool {
        self.bits & flag.bit() != 0
    }

    #[must_use]
    pub fn is_empty(self) -> bool {
        self.bits == 0
    }
}

// ============================================================================
// Mode stack
// ============================================================================

/// Maximum stack depth — beyond this, pushes are rejected.
pub const MAX_STACK_DEPTH: usize = 16;

/// Per-pane keyboard-mode stack.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KittyKbdStack {
    /// Top of the stack is the *last* element. Empty stack
    /// = flag-set 0.
    pub frames: Vec<KittyKbdFlagSet>,
    /// Lifetime push count.
    pub pushes_total: u64,
    /// Lifetime pop count.
    pub pops_total: u64,
    /// Lifetime push-rejected count (overflow guard fired).
    pub pushes_rejected_total: u64,
    /// High-watermark of `frames.len()`.
    pub max_depth_observed: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PushOutcome {
    /// Push accepted; new top equals the pushed flags.
    Pushed,
    /// Stack full — push refused, no state change.
    Rejected,
}

impl KittyKbdStack {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Current top of the stack — `empty()` if no frames.
    #[must_use]
    pub fn current(&self) -> KittyKbdFlagSet {
        self.frames
            .last()
            .copied()
            .unwrap_or(KittyKbdFlagSet::empty())
    }

    #[must_use]
    pub fn depth(&self) -> usize {
        self.frames.len()
    }

    /// Push a new frame. Rejected at `MAX_STACK_DEPTH`.
    pub fn push(&mut self, flags: KittyKbdFlagSet) -> PushOutcome {
        if self.frames.len() >= MAX_STACK_DEPTH {
            self.pushes_rejected_total = self.pushes_rejected_total.saturating_add(1);
            return PushOutcome::Rejected;
        }
        self.frames.push(flags);
        self.pushes_total = self.pushes_total.saturating_add(1);
        if self.frames.len() as u32 > self.max_depth_observed {
            self.max_depth_observed = self.frames.len() as u32;
        }
        PushOutcome::Pushed
    }

    /// Pop one frame. Pop on empty stack is a no-op
    /// (counted as `pops_total += 1` regardless — the
    /// operator submitted a pop op).
    pub fn pop(&mut self) {
        self.pops_total = self.pops_total.saturating_add(1);
        self.frames.pop();
    }
}

// ============================================================================
// CSI parser
// ============================================================================

/// Closed list of Kitty-keyboard CSI sequences.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum KittyKbdCsi {
    /// `CSI > <flags> u`
    Push { flags: KittyKbdFlagSet },
    /// `CSI < u`
    Pop,
    /// `CSI ? u` (query)
    Query,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KittyKbdParseError {
    NotKbdCsi,
    MalformedFlags,
}

/// Parse the body of a CSI sequence (everything between
/// the leading `CSI` / `ESC [` and the final byte).
/// Accepts a body like `> 5 u`, `< u`, `? u`, with
/// optional whitespace around the introducer.
pub fn parse_csi_kbd(body: &str) -> Result<KittyKbdCsi, KittyKbdParseError> {
    let body = body.trim();
    let Some(stripped) = body.strip_suffix('u') else {
        return Err(KittyKbdParseError::NotKbdCsi);
    };
    let stripped = stripped.trim_end();
    if let Some(rest) = stripped.strip_prefix('>') {
        let rest = rest.trim();
        if rest.is_empty() {
            return Ok(KittyKbdCsi::Push {
                flags: KittyKbdFlagSet::empty(),
            });
        }
        let bits: u8 = rest
            .parse()
            .map_err(|_| KittyKbdParseError::MalformedFlags)?;
        return Ok(KittyKbdCsi::Push {
            flags: KittyKbdFlagSet::from_bits_truncate(bits),
        });
    }
    if let Some(rest) = stripped.strip_prefix('<') {
        if !rest.trim().is_empty() {
            return Err(KittyKbdParseError::MalformedFlags);
        }
        return Ok(KittyKbdCsi::Pop);
    }
    if let Some(rest) = stripped.strip_prefix('?') {
        if !rest.trim().is_empty() {
            return Err(KittyKbdParseError::MalformedFlags);
        }
        return Ok(KittyKbdCsi::Query);
    }
    Err(KittyKbdParseError::NotKbdCsi)
}

/// Build the response to a `CSI ? u` query.
#[must_use]
pub fn render_query_response(flags: KittyKbdFlagSet) -> String {
    format!("\x1b[?{}u", flags.bits)
}

// ============================================================================
// Per-mode encoding
// ============================================================================

/// One key event the encoder consumes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct KeyEvent {
    /// Logical key code (e.g., `9` for Tab, `13` for
    /// Enter). Codepoint-flavored — production uses a
    /// richer keysym enum, but the protocol is
    /// codepoint-driven.
    pub key: u32,
    /// Modifier mask (bit 0 = Shift, bit 1 = Ctrl, bit 2 =
    /// Alt, bit 3 = Super).
    pub modifiers: u8,
    /// Event kind — press / release / repeat.
    pub event_kind: KeyEventKind,
    /// Optional alternate key (lowercase / uppercase pair).
    /// Only emitted under flag 4 (`ReportAlternateKeys`).
    pub alternate: Option<u32>,
    /// Optional associated text (IME composition). Only
    /// emitted under flag 16 (`ReportAssociatedText`).
    pub associated_text: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyEventKind {
    Press,
    Repeat,
    Release,
}

impl KeyEventKind {
    /// Numeric event-type code per Kitty protocol:
    /// 1 = press, 2 = repeat, 3 = release.
    #[must_use]
    pub const fn protocol_code(self) -> u8 {
        match self {
            Self::Press => 1,
            Self::Repeat => 2,
            Self::Release => 3,
        }
    }
}

/// Encode a key event into the terminal byte stream under
/// the given flag set.
///
/// Encoding rules (foundation slice — production extends
/// with full keysym tables):
///
/// - **No flags**: legacy ASCII control encoding. Tab → `\t`,
///   Enter → `\r`, Esc → `\x1b`, printable codepoint → its
///   UTF-8 bytes, modified-printable → no-op (legacy chord
///   handling lives in `cooked_input.rs`).
/// - **Flag 1 (Disambiguate)**: keys that legacy collapses
///   onto the same byte (Tab/Ctrl-I, Enter/Ctrl-M,
///   Esc/Ctrl-[) emit `CSI <key> u` instead.
/// - **Flag 2 (ReportEventTypes)**: all encodings include
///   `;<event_code>` after the key field. Press = 1,
///   Repeat = 2, Release = 3.
/// - **Flag 4 (ReportAlternateKeys)**: alt-key pair appended
///   as `:<alt>`.
/// - **Flag 8 (ReportAllKeysAsEscapes)**: all keys —
///   including printable codepoints — emit
///   `CSI <key> ; <event> u`.
/// - **Flag 16 (ReportAssociatedText)**: the
///   `associated_text` field is appended as an extra CSI
///   parameter. (Production routes this through IME
///   composition.)
#[must_use]
pub fn encode_key_event(event: &KeyEvent, flags: KittyKbdFlagSet) -> Vec<u8> {
    // Release events under no-flag mode produce no bytes
    // (legacy doesn't see release).
    if event.event_kind == KeyEventKind::Release && !flags.contains(KittyKbdFlag::ReportEventTypes)
    {
        return Vec::new();
    }

    // Flag 8 (ReportAllKeysAsEscapes) routes everything
    // through the CSI form; Flag 1 (Disambiguate) routes
    // collapsing keys through the CSI form. Flag 16 only needs
    // CSI when there is associated text to carry.
    let needs_csi_form = flags.contains(KittyKbdFlag::ReportAllKeysAsEscapes)
        || (flags.contains(KittyKbdFlag::Disambiguate) && is_collapsing_key(event.key))
        || (flags.contains(KittyKbdFlag::ReportAssociatedText) && event.associated_text.is_some());

    if !needs_csi_form && !flags.contains(KittyKbdFlag::ReportAlternateKeys) {
        return legacy_encoding(event);
    }

    csi_form_encoding(event, flags)
}

/// Keys legacy collapses with control bytes (Tab/Ctrl-I,
/// Enter/Ctrl-M, Esc/Ctrl-[, Backspace/Ctrl-H).
fn is_collapsing_key(key: u32) -> bool {
    matches!(key, 9 | 13 | 27 | 8)
}

fn legacy_encoding(event: &KeyEvent) -> Vec<u8> {
    match event.key {
        9 => b"\t".to_vec(),
        13 => b"\r".to_vec(),
        27 => b"\x1b".to_vec(),
        8 => b"\x08".to_vec(),
        cp if cp >= 0x20 && cp != 0x7f => {
            let mut buf = [0u8; 4];
            let s = char::from_u32(cp).unwrap_or('?').encode_utf8(&mut buf);
            s.as_bytes().to_vec()
        }
        _ => Vec::new(),
    }
}

fn csi_form_encoding(event: &KeyEvent, flags: KittyKbdFlagSet) -> Vec<u8> {
    let mut out = String::from("\x1b[");
    out.push_str(&event.key.to_string());

    if flags.contains(KittyKbdFlag::ReportAlternateKeys) {
        if let Some(alt) = event.alternate {
            out.push(':');
            out.push_str(&alt.to_string());
        }
    }

    let report_events = flags.contains(KittyKbdFlag::ReportEventTypes);
    if report_events || event.modifiers != 0 {
        out.push(';');
        // Modifier param — Kitty packs as
        // `(modifier_mask + 1)`.
        out.push_str(&(event.modifiers as u32 + 1).to_string());
        if report_events {
            out.push(':');
            out.push_str(&event.event_kind.protocol_code().to_string());
        }
    }

    if flags.contains(KittyKbdFlag::ReportAssociatedText) {
        if let Some(ref text) = event.associated_text {
            out.push(';');
            // Codepoints, semicolon-joined.
            let codepoints: Vec<String> = text.chars().map(|c| (c as u32).to_string()).collect();
            out.push_str(&codepoints.join(":"));
        }
    }

    out.push('u');
    out.into_bytes()
}

// ============================================================================
// Health snapshot
// ============================================================================

/// `ft doctor` snapshot. Mirrors the bead's
/// "kbd_mode_pushes / kbd_mode_pops / current_max_depth /
/// kbd_events_by_mode" indicators.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KittyKbdHealth {
    pub current_depth: u32,
    pub max_depth_observed: u32,
    pub pushes_total: u64,
    pub pops_total: u64,
    pub pushes_rejected_total: u64,
    /// Per-flag-set event counter.
    pub events_by_mode: BTreeMap<u8, u64>,
}

impl KittyKbdHealth {
    #[must_use]
    pub fn baseline() -> Self {
        Self {
            current_depth: 0,
            max_depth_observed: 0,
            pushes_total: 0,
            pops_total: 0,
            pushes_rejected_total: 0,
            events_by_mode: BTreeMap::new(),
        }
    }

    /// Project a snapshot from a stack + per-mode event
    /// counts.
    #[must_use]
    pub fn from_stack(stack: &KittyKbdStack, events_by_mode: &BTreeMap<u8, u64>) -> Self {
        Self {
            current_depth: stack.depth() as u32,
            max_depth_observed: stack.max_depth_observed,
            pushes_total: stack.pushes_total,
            pops_total: stack.pops_total,
            pushes_rejected_total: stack.pushes_rejected_total,
            events_by_mode: events_by_mode.clone(),
        }
    }

    /// True iff stack is healthy: depth within limits, no
    /// rejected pushes.
    #[must_use]
    pub fn is_safe(&self) -> bool {
        self.current_depth <= MAX_STACK_DEPTH as u32 && self.pushes_rejected_total == 0
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------------
    // Flag taxonomy
    // ------------------------------------------------------------------------

    #[test]
    fn each_flag_has_distinct_bit() {
        let bits: Vec<u8> = KittyKbdFlag::all().iter().map(|f| f.bit()).collect();
        assert_eq!(bits, vec![1, 2, 4, 8, 16]);
        let mut sorted = bits.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 5);
    }

    #[test]
    fn flag_set_contains_after_set() {
        let mut s = KittyKbdFlagSet::empty();
        s.set(KittyKbdFlag::Disambiguate);
        assert!(s.contains(KittyKbdFlag::Disambiguate));
        assert!(!s.contains(KittyKbdFlag::ReportEventTypes));
    }

    #[test]
    fn flag_set_from_bits_truncates_high_bits() {
        let s = KittyKbdFlagSet::from_bits_truncate(0xFF);
        assert_eq!(s.bits, KittyKbdFlagSet::ALL_BITS);
        assert!(s.contains(KittyKbdFlag::ReportAssociatedText));
    }

    // ------------------------------------------------------------------------
    // Stack
    // ------------------------------------------------------------------------

    #[test]
    fn empty_stack_current_is_no_flags() {
        let s = KittyKbdStack::new();
        assert!(s.current().is_empty());
        assert_eq!(s.depth(), 0);
    }

    #[test]
    fn push_pop_round_trip() {
        let mut s = KittyKbdStack::new();
        let flags = KittyKbdFlagSet::from_bits_truncate(5);
        assert_eq!(s.push(flags), PushOutcome::Pushed);
        assert_eq!(s.depth(), 1);
        assert_eq!(s.current(), flags);
        s.pop();
        assert_eq!(s.depth(), 0);
        assert!(s.current().is_empty());
    }

    #[test]
    fn pop_on_empty_is_noop_but_counted() {
        let mut s = KittyKbdStack::new();
        s.pop();
        assert_eq!(s.depth(), 0);
        assert_eq!(s.pops_total, 1);
    }

    #[test]
    fn nested_push_does_not_affect_outer_scope() {
        let mut s = KittyKbdStack::new();
        let outer = KittyKbdFlagSet::from_bits_truncate(1);
        let inner = KittyKbdFlagSet::from_bits_truncate(15);
        s.push(outer);
        s.push(inner);
        assert_eq!(s.current(), inner);
        s.pop();
        assert_eq!(s.current(), outer);
    }

    #[test]
    fn max_stack_depth_exhausts_at_16() {
        let mut s = KittyKbdStack::new();
        for _ in 0..MAX_STACK_DEPTH {
            assert_eq!(s.push(KittyKbdFlagSet::empty()), PushOutcome::Pushed);
        }
        assert_eq!(s.depth(), MAX_STACK_DEPTH);
        assert_eq!(s.push(KittyKbdFlagSet::empty()), PushOutcome::Rejected);
        assert_eq!(s.pushes_rejected_total, 1);
        assert_eq!(s.depth(), MAX_STACK_DEPTH);
    }

    #[test]
    fn adversarial_thousand_pushes_stops_at_max_no_panic() {
        let mut s = KittyKbdStack::new();
        for _ in 0..1000 {
            s.push(KittyKbdFlagSet::empty());
        }
        assert_eq!(s.depth(), MAX_STACK_DEPTH);
        assert_eq!(s.pushes_rejected_total, 1000 - MAX_STACK_DEPTH as u64);
    }

    #[test]
    fn stress_thousand_push_pop_pairs_stays_in_bounds() {
        let mut s = KittyKbdStack::new();
        for _ in 0..1000 {
            s.push(KittyKbdFlagSet::from_bits_truncate(5));
            s.pop();
        }
        assert_eq!(s.depth(), 0);
        assert_eq!(s.pushes_total, 1000);
        assert_eq!(s.pops_total, 1000);
    }

    #[test]
    fn max_depth_observed_high_watermark() {
        let mut s = KittyKbdStack::new();
        s.push(KittyKbdFlagSet::empty());
        s.push(KittyKbdFlagSet::empty());
        s.push(KittyKbdFlagSet::empty());
        s.pop();
        assert_eq!(s.max_depth_observed, 3);
    }

    // ------------------------------------------------------------------------
    // CSI parser
    // ------------------------------------------------------------------------

    #[test]
    fn parse_push_with_flags() {
        let parsed = parse_csi_kbd("> 5 u").unwrap();
        assert_eq!(
            parsed,
            KittyKbdCsi::Push {
                flags: KittyKbdFlagSet::from_bits_truncate(5),
            }
        );
    }

    #[test]
    fn parse_push_without_flags_is_zero() {
        let parsed = parse_csi_kbd("> u").unwrap();
        assert_eq!(
            parsed,
            KittyKbdCsi::Push {
                flags: KittyKbdFlagSet::empty(),
            }
        );
    }

    #[test]
    fn parse_pop() {
        assert_eq!(parse_csi_kbd("< u").unwrap(), KittyKbdCsi::Pop);
    }

    #[test]
    fn parse_query() {
        assert_eq!(parse_csi_kbd("? u").unwrap(), KittyKbdCsi::Query);
    }

    #[test]
    fn parse_malformed_flags_rejected() {
        assert!(matches!(
            parse_csi_kbd("> abc u"),
            Err(KittyKbdParseError::MalformedFlags)
        ));
    }

    #[test]
    fn parse_pop_with_extra_data_rejected() {
        assert!(matches!(
            parse_csi_kbd("< 5 u"),
            Err(KittyKbdParseError::MalformedFlags)
        ));
    }

    #[test]
    fn parse_non_kbd_csi_rejected() {
        assert!(matches!(
            parse_csi_kbd("31 m"),
            Err(KittyKbdParseError::NotKbdCsi)
        ));
    }

    #[test]
    fn render_query_response_format() {
        let flags = KittyKbdFlagSet::from_bits_truncate(15);
        let rendered = render_query_response(flags);
        assert_eq!(rendered, "\x1b[?15u");
    }

    #[test]
    fn render_query_empty_flags() {
        let rendered = render_query_response(KittyKbdFlagSet::empty());
        assert_eq!(rendered, "\x1b[?0u");
    }

    // ------------------------------------------------------------------------
    // Encoding
    // ------------------------------------------------------------------------

    fn press(key: u32) -> KeyEvent {
        KeyEvent {
            key,
            modifiers: 0,
            event_kind: KeyEventKind::Press,
            alternate: None,
            associated_text: None,
        }
    }

    #[test]
    fn legacy_tab_emits_horizontal_tab_byte() {
        let bytes = encode_key_event(&press(9), KittyKbdFlagSet::empty());
        assert_eq!(bytes, b"\t");
    }

    #[test]
    fn legacy_enter_emits_carriage_return() {
        let bytes = encode_key_event(&press(13), KittyKbdFlagSet::empty());
        assert_eq!(bytes, b"\r");
    }

    #[test]
    fn legacy_release_event_emits_nothing() {
        let mut ev = press(9);
        ev.event_kind = KeyEventKind::Release;
        let bytes = encode_key_event(&ev, KittyKbdFlagSet::empty());
        assert!(bytes.is_empty());
    }

    #[test]
    fn disambiguate_tab_emits_csi_form() {
        let mut flags = KittyKbdFlagSet::empty();
        flags.set(KittyKbdFlag::Disambiguate);
        let bytes = encode_key_event(&press(9), flags);
        assert_eq!(bytes, b"\x1b[9u");
    }

    #[test]
    fn disambiguate_does_not_affect_printable_keys() {
        let mut flags = KittyKbdFlagSet::empty();
        flags.set(KittyKbdFlag::Disambiguate);
        let bytes = encode_key_event(&press(b'a' as u32), flags);
        // 'a' is printable + non-collapsing → legacy
        // encoding still applies under flag 1 alone.
        assert_eq!(bytes, b"a");
    }

    #[test]
    fn report_all_keys_routes_printable_through_csi() {
        let mut flags = KittyKbdFlagSet::empty();
        flags.set(KittyKbdFlag::ReportAllKeysAsEscapes);
        let bytes = encode_key_event(&press(b'a' as u32), flags);
        assert_eq!(bytes, b"\x1b[97u");
    }

    #[test]
    fn report_event_types_includes_event_code() {
        let mut flags = KittyKbdFlagSet::empty();
        flags.set(KittyKbdFlag::ReportAllKeysAsEscapes);
        flags.set(KittyKbdFlag::ReportEventTypes);
        let bytes = encode_key_event(&press(b'a' as u32), flags);
        // mod=0+1=1, event=press=1
        assert_eq!(bytes, b"\x1b[97;1:1u");
    }

    #[test]
    fn report_event_types_release_emitted() {
        let mut flags = KittyKbdFlagSet::empty();
        flags.set(KittyKbdFlag::ReportAllKeysAsEscapes);
        flags.set(KittyKbdFlag::ReportEventTypes);
        let mut ev = press(b'a' as u32);
        ev.event_kind = KeyEventKind::Release;
        let bytes = encode_key_event(&ev, flags);
        assert_eq!(bytes, b"\x1b[97;1:3u");
    }

    #[test]
    fn report_alternate_keys_appends_pair() {
        let mut flags = KittyKbdFlagSet::empty();
        flags.set(KittyKbdFlag::ReportAlternateKeys);
        flags.set(KittyKbdFlag::ReportAllKeysAsEscapes);
        let mut ev = press(b'a' as u32);
        ev.alternate = Some(b'A' as u32);
        let bytes = encode_key_event(&ev, flags);
        assert_eq!(bytes, b"\x1b[97:65u");
    }

    #[test]
    fn report_associated_text_appends_codepoints() {
        let mut flags = KittyKbdFlagSet::empty();
        flags.set(KittyKbdFlag::ReportAllKeysAsEscapes);
        flags.set(KittyKbdFlag::ReportAssociatedText);
        let mut ev = press(b'a' as u32);
        ev.associated_text = Some("ab".to_string());
        let bytes = encode_key_event(&ev, flags);
        // Final form: CSI 97 ; 97:98 u
        assert_eq!(bytes, b"\x1b[97;97:98u");
    }

    #[test]
    fn report_associated_text_alone_forces_csi_form() {
        let mut flags = KittyKbdFlagSet::empty();
        flags.set(KittyKbdFlag::ReportAssociatedText);
        let mut ev = press(b'a' as u32);
        ev.associated_text = Some("x".to_string());
        let bytes = encode_key_event(&ev, flags);
        assert_eq!(bytes, b"\x1b[97;120u");
    }

    #[test]
    fn modifiers_are_encoded_when_nonzero() {
        let mut flags = KittyKbdFlagSet::empty();
        flags.set(KittyKbdFlag::ReportAllKeysAsEscapes);
        let mut ev = press(b'a' as u32);
        ev.modifiers = 0b0010; // Ctrl
        let bytes = encode_key_event(&ev, flags);
        // mod=2+1=3
        assert_eq!(bytes, b"\x1b[97;3u");
    }

    // ------------------------------------------------------------------------
    // Health snapshots
    // ------------------------------------------------------------------------

    #[test]
    fn health_baseline_safe() {
        assert!(KittyKbdHealth::baseline().is_safe());
    }

    #[test]
    fn health_unsafe_when_pushes_rejected() {
        let mut s = KittyKbdStack::new();
        for _ in 0..=MAX_STACK_DEPTH {
            s.push(KittyKbdFlagSet::empty());
        }
        let h = KittyKbdHealth::from_stack(&s, &BTreeMap::new());
        assert!(!h.is_safe());
        assert_eq!(h.pushes_rejected_total, 1);
    }

    #[test]
    fn health_records_max_depth() {
        let mut s = KittyKbdStack::new();
        for _ in 0..5 {
            s.push(KittyKbdFlagSet::from_bits_truncate(1));
        }
        s.pop();
        let h = KittyKbdHealth::from_stack(&s, &BTreeMap::new());
        assert_eq!(h.current_depth, 4);
        assert_eq!(h.max_depth_observed, 5);
    }

    // ------------------------------------------------------------------------
    // Bead's headline scenario
    // ------------------------------------------------------------------------

    #[test]
    fn vim_progressive_enhancement_scenario() {
        // Vim pushes mode 5 (Disambiguate +
        // ReportAlternateKeys), distinguishes Tab from
        // Ctrl-I, then pops on exit.
        let mut s = KittyKbdStack::new();

        // Vim's startup sequence:
        let parsed = parse_csi_kbd("> 5 u").unwrap();
        let KittyKbdCsi::Push { flags } = parsed else {
            panic!("expected push");
        };
        assert_eq!(s.push(flags), PushOutcome::Pushed);

        // Tab now disambiguates:
        let bytes = encode_key_event(&press(9), s.current());
        assert_eq!(bytes, b"\x1b[9u");

        // Vim exits:
        let parsed = parse_csi_kbd("< u").unwrap();
        assert_eq!(parsed, KittyKbdCsi::Pop);
        s.pop();

        // Back to legacy: Tab → \t.
        let bytes = encode_key_event(&press(9), s.current());
        assert_eq!(bytes, b"\t");
    }

    #[test]
    fn full_serde_roundtrip() {
        let mut s = KittyKbdStack::new();
        s.push(KittyKbdFlagSet::from_bits_truncate(5));
        s.push(KittyKbdFlagSet::from_bits_truncate(15));
        let json = serde_json::to_string(&s).unwrap();
        let parsed: KittyKbdStack = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, s);
    }
}
