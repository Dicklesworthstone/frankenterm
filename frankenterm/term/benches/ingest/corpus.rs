//! Deterministic corpus generator for the headless ingest throughput bench.
//!
//! Every corpus is a pure function of `(corpus, size, seed)`: the same triple
//! yields byte-identical output on every platform, and the smoke test pins a
//! SHA-256 per corpus. Output is cut to exactly `size` bytes, the same
//! `head -c SIZE` slicing the planning measurements used, so the final frame or
//! escape sequence may be torn.
//!
//! This file depends on `std` only.

use std::convert::TryFrom;

/// Bump whenever any corpus's output bytes change, so the on-disk cache
/// regenerates corpora written by an older generator instead of reusing them.
/// The SHA-256 pins in `tests/ingest_throughput_smoke.rs` fail on such a
/// change and point here.
pub const GENERATOR_VERSION: u32 = 1;

/// Seed used when the caller does not pass one.
pub const DEFAULT_SEED: u64 = 20_261_005;

/// Default corpus size: the 64 MiB slice the planning measurements used.
pub const DEFAULT_SIZE: usize = 64 * 1024 * 1024;

/// Screen geometry the `tui_repaint` frames are drawn for. It matches the
/// lanes' default `ROWS` x `COLS`; on other geometries cursor addressing clamps.
pub const TUI_ROWS: usize = 80;
pub const TUI_COLS: usize = 120;

/// Code point ranges of the operator's emoji pool, in the script's order.
pub const EMOJI_RANGES: [(u32, u32); 5] = [
    (0x1F600, 0x1F64F),
    (0x1F300, 0x1F5FF),
    (0x1F680, 0x1F6FF),
    (0x1F900, 0x1F9FF),
    (0x1FA70, 0x1FAFF),
];

/// The operator script's ASCII pool, verbatim. It has 71 characters (there is
/// no `$`), so the full pool holds 1,376 emoji + 71 = 1,447 entries.
pub const EMOJI_ASCII_POOL: &str =
    "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!@#%^&*()";

/// `color_random`'s character class, `[A-Za-z0-9!@#$%^&*()]` (72 characters).
pub const COLOR_RANDOM_POOL: &[u8] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!@#$%^&*()";

const SOURCE_WORDS: &[&str] = &[
    "fn", "let", "mut", "self", "impl", "match", "return", "if", "else", "for", "in", "while",
    "pub", "struct", "enum", "use", "crate", "terminal", "cursor", "line", "cell", "width",
    "attrs", "parser", "action", "screen", "scrollback", "config", "Arc", "Vec", "Option",
    "Some", "None", "Ok", "Err", "usize", "u8", "String", "clone", "unwrap_or", "len", "push",
    "iter", "map", "collect", "seqno", "grapheme", "palette",
];

const SOURCE_PUNCT: &[&str] = &[
    "(", ")", "{", "}", "[", "]", ";", ",", ".", "::", "->", "=>", "=", "==", "!=", "<", ">",
    "+", "-", "*", "/", "&", "&&", "||", "!", "?", "#", "\"", "'", ":",
];

/// Emoji ZWJ sequences for `unicode_mix`.
const ZWJ_SEQUENCES: &[&str] = &[
    // family: man, woman, girl
    "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}",
    // rainbow flag
    "\u{1F3F3}\u{FE0F}\u{200D}\u{1F308}",
    // woman technologist, medium skin tone
    "\u{1F469}\u{1F3FD}\u{200D}\u{1F4BB}",
    // heart on fire
    "\u{2764}\u{FE0F}\u{200D}\u{1F525}",
    // people holding hands
    "\u{1F9D1}\u{200D}\u{1F91D}\u{200D}\u{1F9D1}",
    // polar bear
    "\u{1F43B}\u{200D}\u{2744}\u{FE0F}",
];

/// Braille spinner frames, as agent TUIs draw them.
const SPINNER: [&str; 10] = [
    "\u{280B}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283C}", "\u{2834}", "\u{2826}",
    "\u{2827}", "\u{2807}", "\u{280F}",
];

const BOX_HORIZONTAL: &str = "\u{2500}";
const BOX_VERTICAL: &str = "\u{2502}";
const BOX_TOP_LEFT: &str = "\u{256D}";
const BOX_TOP_RIGHT: &str = "\u{256E}";
const BOX_BOTTOM_LEFT: &str = "\u{2570}";
const BOX_BOTTOM_RIGHT: &str = "\u{256F}";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Corpus {
    /// The operator's primary test (scoreboard T0), format-exact: per frame
    /// `ESC[38;5;{fg}m ESC[48;5;{bg}m {char}`.
    ColorEmojiRandom,
    /// `ESC[38;5;{n}m` plus one character: the operator's first incident workload.
    ColorRandom,
    /// Decimal integers from 1, one per `\n`: byte-identical to
    /// `seq 1 N | head -c SIZE`, the planning session's seq corpus, and fed
    /// raw (no ONLCR) exactly as ghostty-bench consumed it.
    SeqLines,
    /// Source-code-like ASCII lines of 80-300 columns with tabs, `\r\n` ended
    /// as a PTY with ONLCR delivers them.
    LongLines,
    /// CJK wide characters, combining marks, emoji ZWJ sequences, regional
    /// indicator flags and RTL fragments, `\r\n` ended.
    UnicodeMix,
    /// Full-screen agent-TUI frames: cursor addressing, SGR runs, box drawing
    /// and partial line erases.
    TuiRepaint,
}

impl Corpus {
    /// Every corpus; the operator's primary test comes first.
    pub const ALL: [Corpus; 6] = [
        Corpus::ColorEmojiRandom,
        Corpus::ColorRandom,
        Corpus::SeqLines,
        Corpus::LongLines,
        Corpus::UnicodeMix,
        Corpus::TuiRepaint,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Corpus::ColorEmojiRandom => "color_emoji_random",
            Corpus::ColorRandom => "color_random",
            Corpus::SeqLines => "seq_lines",
            Corpus::LongLines => "long_lines",
            Corpus::UnicodeMix => "unicode_mix",
            Corpus::TuiRepaint => "tui_repaint",
        }
    }

    /// Accepts the canonical name with `-` or `_` separators.
    pub fn from_name(name: &str) -> Option<Self> {
        let normalized = name.replace('-', "_");
        Self::ALL.iter().copied().find(|corpus| corpus.name() == normalized)
    }

    /// Whether the corpus moves the cursor only by printing, CR and LF. Line
    /// feeds then give a lower bound on the rows a terminal must retain;
    /// corpora with cursor addressing have no such bound.
    pub fn is_line_oriented(self) -> bool {
        !matches!(self, Corpus::TuiRepaint)
    }

    /// Generates exactly `size` bytes. `seq_lines` ignores the seed.
    pub fn generate(self, size: usize, seed: u64) -> Vec<u8> {
        let mut rng = Rng::new(seed);
        // Generators stop after the frame that crosses `size`; leave room for it.
        let mut out = Vec::with_capacity(size.saturating_add(16 * 1024));
        match self {
            Corpus::ColorEmojiRandom => color_emoji_random(&mut out, size, &mut rng),
            Corpus::ColorRandom => color_random(&mut out, size, &mut rng),
            Corpus::SeqLines => seq_lines(&mut out, size),
            Corpus::LongLines => long_lines(&mut out, size, &mut rng),
            Corpus::UnicodeMix => unicode_mix(&mut out, size, &mut rng),
            Corpus::TuiRepaint => tui_repaint(&mut out, size, &mut rng),
        }
        out.truncate(size);
        out
    }
}

/// xorshift64* seeded through splitmix64: small, fast and fully specified, so a
/// corpus depends on nothing but its seed.
#[derive(Clone, Debug)]
pub struct Rng {
    state: u64,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        // Zero is xorshift's fixed point.
        let state = if z == 0 { 0x9E37_79B9_7F4A_7C15 } else { z };
        Self { state }
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Draws from `0..n` by multiply-shift; the bias is at most `n / 2^64`.
    pub fn below(&mut self, n: usize) -> usize {
        assert!(n > 0, "Rng::below needs a non-empty range");
        let n = u64::try_from(n).expect("range bound fits in u64");
        let scaled = (u128::from(self.next_u64()) * u128::from(n)) >> 64;
        usize::try_from(scaled).expect("multiply-shift result is below n")
    }

    /// Draws from `lo..=hi`.
    pub fn range_inclusive(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below(hi - lo + 1)
    }
}

/// The operator's pool: the 1,376 emoji in script order, then the 71 ASCII
/// characters.
pub fn emoji_pool() -> Vec<char> {
    let mut pool = Vec::with_capacity(1447);
    for &(start, end) in EMOJI_RANGES.iter() {
        for cp in start..=end {
            pool.push(char::from_u32(cp).expect("emoji pool ranges hold no surrogates"));
        }
    }
    pool.extend(EMOJI_ASCII_POOL.chars());
    pool
}

fn push_decimal(out: &mut Vec<u8>, mut n: usize) {
    let mut digits = [0_u8; 20];
    let mut at = digits.len();
    loop {
        at -= 1;
        digits[at] = b'0' + u8::try_from(n % 10).expect("a decimal digit fits in u8");
        n /= 10;
        if n == 0 {
            break;
        }
    }
    out.extend_from_slice(&digits[at..]);
}

/// Appends `{prefix}{n}m`, e.g. `ESC[38;5;` + `n` + `m`.
fn push_sgr(out: &mut Vec<u8>, prefix: &[u8], n: usize) {
    out.extend_from_slice(prefix);
    push_decimal(out, n);
    out.push(b'm');
}

fn push_cup(out: &mut Vec<u8>, row: usize, col: usize) {
    out.extend_from_slice(b"\x1b[");
    push_decimal(out, row);
    out.push(b';');
    push_decimal(out, col);
    out.push(b'H');
}

fn push_code_point(out: &mut Vec<u8>, rng: &mut Rng, base: u32, count: usize) {
    let offset = u32::try_from(rng.below(count)).expect("code point offset fits in u32");
    let ch = char::from_u32(base + offset).expect("generator code points are scalar values");
    let mut buf = [0_u8; 4];
    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
}

fn color_emoji_random(out: &mut Vec<u8>, size: usize, rng: &mut Rng) {
    let pool: Vec<Vec<u8>> = emoji_pool()
        .into_iter()
        .map(|ch| ch.to_string().into_bytes())
        .collect();
    // Same draw order as the script: fg, bg, then the character.
    while out.len() < size {
        let fg = rng.below(256);
        let bg = rng.below(256);
        let glyph = &pool[rng.below(pool.len())];
        push_sgr(out, b"\x1b[38;5;", fg);
        push_sgr(out, b"\x1b[48;5;", bg);
        out.extend_from_slice(glyph);
    }
}

fn color_random(out: &mut Vec<u8>, size: usize, rng: &mut Rng) {
    while out.len() < size {
        let color = rng.below(256);
        let ch = COLOR_RANDOM_POOL[rng.below(COLOR_RANDOM_POOL.len())];
        push_sgr(out, b"\x1b[38;5;", color);
        out.push(ch);
    }
}

fn seq_lines(out: &mut Vec<u8>, size: usize) {
    let mut n = 1;
    while out.len() < size {
        push_decimal(out, n);
        out.push(b'\n');
        n += 1;
    }
}

fn long_lines(out: &mut Vec<u8>, size: usize, rng: &mut Rng) {
    let mut line = Vec::with_capacity(320);
    while out.len() < size {
        line.clear();
        let width = rng.range_inclusive(80, 300);
        if rng.below(4) == 0 {
            let tabs = rng.range_inclusive(1, 3);
            line.resize(tabs, b'\t');
        } else {
            let spaces = rng.below(4) * 4;
            line.resize(spaces, b' ');
        }
        // At most one tab-aligned trailing comment per line.
        let mut in_comment = false;
        while line.len() < width {
            match rng.below(16) {
                0..=8 => {
                    let word = SOURCE_WORDS[rng.below(SOURCE_WORDS.len())];
                    line.extend_from_slice(word.as_bytes());
                }
                9..=12 => {
                    let punct = SOURCE_PUNCT[rng.below(SOURCE_PUNCT.len())];
                    line.extend_from_slice(punct.as_bytes());
                }
                13 | 14 => {
                    let number = rng.below(100_000);
                    push_decimal(&mut line, number);
                }
                _ if !in_comment => {
                    line.extend_from_slice(b"\t// ");
                    in_comment = true;
                    continue;
                }
                _ => {
                    let word = SOURCE_WORDS[rng.below(SOURCE_WORDS.len())];
                    line.extend_from_slice(word.as_bytes());
                }
            }
            if rng.below(3) != 0 {
                line.push(b' ');
            }
        }
        line.truncate(width);
        out.extend_from_slice(&line);
        out.extend_from_slice(b"\r\n");
    }
}

fn unicode_mix(out: &mut Vec<u8>, size: usize, rng: &mut Rng) {
    while out.len() < size {
        let segments = rng.range_inclusive(6, 18);
        for segment in 0..segments {
            if segment > 0 {
                out.push(b' ');
            }
            match rng.below(6) {
                0 => {
                    let word = SOURCE_WORDS[rng.below(SOURCE_WORDS.len())];
                    out.extend_from_slice(word.as_bytes());
                }
                1 => {
                    // CJK Unified Ideographs U+4E00..=U+9FFF: double-width.
                    for _ in 0..rng.range_inclusive(1, 6) {
                        push_code_point(out, rng, 0x4E00, 0x5200);
                    }
                }
                2 => {
                    // Latin letters with combining diacritics U+0300..=U+036F.
                    for _ in 0..rng.range_inclusive(1, 4) {
                        out.push(b'a' + u8::try_from(rng.below(26)).expect("letter offset"));
                        for _ in 0..rng.range_inclusive(1, 2) {
                            push_code_point(out, rng, 0x0300, 0x70);
                        }
                    }
                }
                3 => {
                    let sequence = ZWJ_SEQUENCES[rng.below(ZWJ_SEQUENCES.len())];
                    out.extend_from_slice(sequence.as_bytes());
                }
                4 => {
                    // Hebrew U+05D0..=U+05EA or Arabic U+0627..=U+064A letters.
                    let (base, count) = if rng.below(2) == 0 {
                        (0x05D0, 27)
                    } else {
                        (0x0627, 36)
                    };
                    for _ in 0..rng.range_inclusive(2, 8) {
                        push_code_point(out, rng, base, count);
                    }
                }
                _ => {
                    // A regional indicator pair (a flag).
                    push_code_point(out, rng, 0x1F1E6, 26);
                    push_code_point(out, rng, 0x1F1E6, 26);
                }
            }
        }
        out.extend_from_slice(b"\r\n");
    }
}

fn push_box_rule(out: &mut Vec<u8>, row: usize, left: &str, right: &str) {
    push_cup(out, row, 1);
    out.extend_from_slice(b"\x1b[38;5;240m");
    out.extend_from_slice(left.as_bytes());
    for _ in 0..TUI_COLS - 2 {
        out.extend_from_slice(BOX_HORIZONTAL.as_bytes());
    }
    out.extend_from_slice(right.as_bytes());
    out.extend_from_slice(b"\x1b[0m");
}

fn tui_repaint(out: &mut Vec<u8>, size: usize, rng: &mut Rng) {
    let mut frame = 0;
    while out.len() < size {
        out.extend_from_slice(b"\x1b[?25l");

        // Title row.
        push_cup(out, 1, 1);
        push_sgr(out, b"\x1b[1;38;5;", rng.below(256));
        out.extend_from_slice(" agent session \u{00B7} frame ".as_bytes());
        push_decimal(out, frame);
        out.extend_from_slice(b"\x1b[0m\x1b[K");

        push_box_rule(out, 2, BOX_TOP_LEFT, BOX_TOP_RIGHT);
        for row in 3..=TUI_ROWS - 3 {
            push_cup(out, row, 1);
            out.extend_from_slice(b"\x1b[38;5;240m");
            out.extend_from_slice(BOX_VERTICAL.as_bytes());
            out.extend_from_slice(b"\x1b[0m ");
            // A partial line of colored word runs; the words are ASCII, so
            // bytes are columns.
            let budget = rng.below(TUI_COLS - 3);
            let mut used = 0;
            while used < budget {
                push_sgr(out, b"\x1b[38;5;", rng.below(256));
                if rng.below(8) == 0 {
                    out.extend_from_slice(b"\x1b[1m");
                }
                let word = SOURCE_WORDS[rng.below(SOURCE_WORDS.len())].as_bytes();
                let take = word.len().min(budget - used);
                out.extend_from_slice(&word[..take]);
                used += take;
                if used < budget {
                    out.push(b' ');
                    used += 1;
                }
                out.extend_from_slice(b"\x1b[0m");
            }
            // Partial line erase: clear what the previous frame left.
            out.extend_from_slice(b"\x1b[K");
            push_cup(out, row, TUI_COLS);
            out.extend_from_slice(b"\x1b[38;5;240m");
            out.extend_from_slice(BOX_VERTICAL.as_bytes());
            out.extend_from_slice(b"\x1b[0m");
        }
        push_box_rule(out, TUI_ROWS - 2, BOX_BOTTOM_LEFT, BOX_BOTTOM_RIGHT);

        // Status row with a spinner; the agent repaints it every frame.
        push_cup(out, TUI_ROWS - 1, 1);
        out.extend_from_slice(b"\x1b[38;5;214m");
        out.extend_from_slice(SPINNER[frame % SPINNER.len()].as_bytes());
        out.extend_from_slice(" Thinking\u{2026} (".as_bytes());
        push_decimal(out, frame / 30);
        out.extend_from_slice(" s \u{00B7} esc to interrupt)\x1b[0m\x1b[K".as_bytes());

        // Input row: erase it, draw the prompt, show the cursor after it.
        push_cup(out, TUI_ROWS, 1);
        out.extend_from_slice(b"\x1b[2K> \x1b[?25h");
        frame += 1;
    }
}
