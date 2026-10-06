//! Byte streams and chunk plans: the M.1 corpora, the escape-parser fuzz
//! seeds, hand-written adversarial cases and seeded random streams.

use std::path::Path;

// Braced imports never mix naming cases: edition 2018 and 2024 rustfmt sort
// those differently, and this module is formatted under both.
use super::corpus::{self, Corpus, Rng};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChunkPlan {
    /// One chunk.
    Whole,
    /// One byte per chunk: every escape and UTF-8 sequence is split.
    Bytewise,
    /// Random chunk sizes in `1..=max`.
    Random { max: usize },
}

pub const CHUNK_PLANS: [ChunkPlan; 5] = [
    ChunkPlan::Whole,
    ChunkPlan::Bytewise,
    ChunkPlan::Random { max: 7 },
    ChunkPlan::Random { max: 64 },
    ChunkPlan::Random { max: 4096 },
];

pub fn chunk(bytes: &[u8], plan: ChunkPlan, seed: u64) -> Vec<Vec<u8>> {
    match plan {
        ChunkPlan::Whole => vec![bytes.to_vec()],
        ChunkPlan::Bytewise => bytes.iter().map(|&byte| vec![byte]).collect(),
        ChunkPlan::Random { max } => {
            let mut rng = Rng::new(seed);
            let mut chunks = Vec::new();
            let mut rest = bytes;
            while !rest.is_empty() {
                let size = rng.range_inclusive(1, max.max(1)).min(rest.len());
                let (head, tail) = rest.split_at(size);
                chunks.push(head.to_vec());
                rest = tail;
            }
            chunks
        }
    }
}

/// The plan for a seed, cycling through every plan.
pub fn chunk_with_seed(bytes: &[u8], seed: u64) -> Vec<Vec<u8>> {
    let plan = CHUNK_PLANS[(seed % CHUNK_PLANS.len() as u64) as usize];
    chunk(bytes, plan, seed)
}

/// Every split of `bytes` into two chunks.
pub fn every_split(bytes: &[u8]) -> Vec<Vec<Vec<u8>>> {
    (1..bytes.len())
        .map(|at| vec![bytes[..at].to_vec(), bytes[at..].to_vec()])
        .collect()
}

/// The M.1 ingest-bench corpora (ft-yccm0.1.2) at `size` bytes.
pub fn m1_corpora(size: usize) -> Vec<(&'static str, Vec<u8>)> {
    Corpus::ALL
        .iter()
        .map(|kind| (kind.name(), kind.generate(size, corpus::DEFAULT_SEED)))
        .collect()
}

/// The escape-parser fuzz seeds (`fuzz/corpus/escape_parser_raw`), sorted by
/// file name.
pub fn escape_parser_seeds(dir: &Path) -> std::io::Result<Vec<(String, Vec<u8>)>> {
    let mut seeds = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            let name = entry.file_name().to_string_lossy().into_owned();
            seeds.push((name, std::fs::read(entry.path())?));
        }
    }
    seeds.sort();
    Ok(seeds)
}

fn repeat(piece: &str, times: usize) -> String {
    let mut out = String::with_capacity(piece.len() * times);
    for _ in 0..times {
        out.push_str(piece);
    }
    out
}

/// Hand-written adversarial cases: split UTF-8 and escapes at chunk edges,
/// invalid UTF-8, C1 controls, huge and numerous parameters, oversized
/// strings, and the mode and margin machinery.
pub fn adversarial_cases() -> Vec<(&'static str, Vec<u8>)> {
    let mut cases: Vec<(&'static str, Vec<u8>)> = vec![
        ("utf8_4byte", "ab\u{1F600}cd\u{1FAE0}".as_bytes().to_vec()),
        ("utf8_cjk", "\u{4E2D}\u{6587}x\u{5B57}".as_bytes().to_vec()),
        ("combining", "e\u{301}\u{302}x\u{0332}".as_bytes().to_vec()),
        (
            "zwj_family",
            "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}!".as_bytes().to_vec(),
        ),
        (
            "regional_indicators",
            "\u{1F1FA}\u{1F1F8}\u{1F1EB}\u{1F1F7}".as_bytes().to_vec(),
        ),
        ("vs16", "\u{2764}\u{FE0F}x\u{263A}\u{FE0F}".as_bytes().to_vec()),
        ("csi_sgr_256", b"\x1b[38;5;196mX\x1b[48;5;21mY\x1b[0mZ".to_vec()),
        ("csi_sgr_truecolor", b"\x1b[38;2;1;2;3mT\x1b[58;2;4;5;6m\x1b[4:3mU\x1b[m".to_vec()),
        ("osc_title_bel", b"\x1b]0;hello title\x07after".to_vec()),
        ("osc_title_st", b"\x1b]2;title two\x1b\\after".to_vec()),
        (
            "osc8_hyperlink",
            b"\x1b]8;id=x;https://example.com/a\x1b\\link\x1b]8;;\x1b\\ plain".to_vec(),
        ),
        (
            "osc133_semantic",
            b"\x1b]133;A\x07$ \x1b]133;B\x07ls\x1b]133;C\x07\r\nout\x1b]133;D;0\x07".to_vec(),
        ),
        ("dcs_decrqss", b"\x1bP$qm\x1b\\after".to_vec()),
        (
            "sixel_small",
            b"\x1bPq#0;2;0;0;0#1;2;100;100;0#1~~@@vv@@~~\x1b\\after".to_vec(),
        ),
        (
            "invalid_utf8",
            vec![
                0xFF, b'a', 0xC0, 0xAF, b'b', 0x80, b'c', 0xE2, 0x82, b'd', 0xED, 0xA0, 0x80, b'e',
                0xF4, 0x90, 0x80, 0x80, b'f',
            ],
        ),
        // ft-fjxga: the byte that breaks a UTF-8 sequence is parsed again,
        // so an ESC, an OSC terminator or a control after it survives.
        (
            "utf8_truncated_then_csi",
            b"a\xe4\x1b[1mb\xf0\x9f\x98\x1b[0mc".to_vec(),
        ),
        (
            "utf8_truncated_in_osc_then_bel",
            b"\x1b]0;t\xe4\x07after".to_vec(),
        ),
        (
            "utf8_truncated_then_controls",
            b"x\xc3\r\ny\xe2\x82\tz".to_vec(),
        ),
        ("latin1_gr_bytes", (0xa0..=0xffu8).collect()),
        (
            "c1_controls_utf8",
            "a\u{84}b\u{85}c\u{9b}31md\u{9d}0;t\u{9c}e".as_bytes().to_vec(),
        ),
        ("c1_controls_raw", vec![0x84, 0x85, 0x9b, b'3', b'1', b'm', b'x']),
        (
            "huge_params",
            b"\x1b[99999999999999999999999mX\x1b[4294967296AY\x1b[65536;65536HZ".to_vec(),
        ),
        (
            "decaln_and_margins",
            b"\x1b#8\x1b[3;4r\x1b[5;1HXY\x1b[2S\x1b[1T\x1b[r\x1b[H".to_vec(),
        ),
        (
            "decslrm_band",
            b"\x1b[?69h\x1b[3;6s\x1b[2;3HABCDEFGH\x1b[2@\x1b[1P\x1b[2X\x1b[?69l".to_vec(),
        ),
        (
            "alt_screen_switch",
            b"main\x1b[?1049halt\x1b[2J\x1b[?1049lback\x1b[?47hx\x1b[?47l".to_vec(),
        ),
        (
            "insert_and_origin",
            b"\x1b[4hins\x1b[4l\x1b[?6h\x1b[2;4r\x1b[Hor\x1b[?6l\x1b[r".to_vec(),
        ),
        ("charsets", b"\x1b(0lqqk\x1b(B\x0eabc\x0f\x1b)0\x0eq\x0f".to_vec()),
        ("tabs_and_rep", b"a\tb\x1b[3gc\x1bHd\tX\x1b[5bY\x1b[2Z".to_vec()),
        (
            "wide_at_margin",
            "12345678\u{4E2D}9\r\n1234567\u{1F600}\u{1F600}".as_bytes().to_vec(),
        ),
        ("reverse_wrap", b"\x1b[?45habcdefghijk\r\x08\x08X\x1b[?45l".to_vec()),
        (
            "keyboard_and_mouse_modes",
            b"\x1b[>1u\x1b[?2004h\x1b[?1000h\x1b[?1006h\x1b[?1004h\x1b[<u\x1b[?1000l".to_vec(),
        ),
        ("device_queries", b"\x1b[6n\x1b[c\x1b[>c\x1b[5n".to_vec()),
        (
            "unicode_version",
            "\x1b]1337;UnicodeVersion=14\x07\u{1FAE0}\x1b]1337;UnicodeVersion=push\x07x\x1b]1337;UnicodeVersion=pop\x07"
                .as_bytes()
                .to_vec(),
        ),
        ("resets", b"\x1b[1;31mX\x1bc after RIS\x1b[1mB\x1b[!p soft".to_vec()),
        ("save_restore_cursor", b"ab\x1b7\x1b[3;3Hcd\x1b8ef\x1b[sgh\x1b[uij".to_vec()),
        ("index_and_reverse_index", b"top\x1bD\x1bD\x1bMmid\x1bEnext\x1b[1;1H\x1bMup".to_vec()),
        ("erase_variants", b"abcdef\x1b[3D\x1b[K\x1b[1K\x1b[2Kx\x1b[J\x1b[1J\x1b[3J".to_vec()),
    ];

    // Large cases: chunk plans other than whole and random are too slow here.
    let mut many_params = b"\x1b[".to_vec();
    many_params.extend_from_slice(repeat("1;", 2000).as_bytes());
    many_params.extend_from_slice(b"mZ");
    cases.push(("many_params", many_params));

    let mut long_osc = b"\x1b]0;".to_vec();
    long_osc.extend_from_slice(repeat("t", 70_000).as_bytes());
    long_osc.extend_from_slice(b"\x07after");
    cases.push(("long_osc_title", long_osc));

    let mut long_line = repeat("wrap me ", 300).into_bytes();
    long_line.extend_from_slice(b"\r\n");
    cases.push(("long_wrapped_line", long_line));

    cases
}

/// Dictionary pieces for random streams. `fuzz/term_engine_differential.dict`
/// holds the same tokens for libFuzzer.
pub const DICTIONARY: &[&[u8]] = &[
    b"\x1b",
    b"\x1b[",
    b"\x1b]",
    b"\x1bP",
    b"\x1b\\",
    b"\x07",
    b"\r\n",
    b"\n",
    b"\r",
    b"\t",
    b"\x08",
    b"\x1b[m",
    b"\x1b[0m",
    b"\x1b[1m",
    b"\x1b[7m",
    b"\x1b[4:3m",
    b"\x1b[38;5;",
    b"\x1b[48;5;",
    b"\x1b[38;2;",
    b"\x1b[K",
    b"\x1b[2J",
    b"\x1b[H",
    b"\x1b[3;5r",
    b"\x1b[r",
    b"\x1b[2S",
    b"\x1b[T",
    b"\x1b[@",
    b"\x1b[P",
    b"\x1b[L",
    b"\x1b[M",
    b"\x1b[X",
    b"\x1b[4h",
    b"\x1b[4l",
    b"\x1b[?6h",
    b"\x1b[?6l",
    b"\x1b[?7l",
    b"\x1b[?7h",
    b"\x1b[?69h",
    b"\x1b[2;4s",
    b"\x1b[?1049h",
    b"\x1b[?1049l",
    b"\x1b[?25l",
    b"\x1b[?25h",
    b"\x1b(0",
    b"\x1b(B",
    b"\x1b7",
    b"\x1b8",
    b"\x1bD",
    b"\x1bM",
    b"\x1b#8",
    b"\x1b]0;",
    b"\x1b]8;;",
    b"\x1b]133;A\x07",
    "\u{1F600}".as_bytes(),
    "\u{4E2D}".as_bytes(),
    "\u{301}".as_bytes(),
    "\u{200D}".as_bytes(),
    "\u{FE0F}".as_bytes(),
    "\u{1F1FA}".as_bytes(),
    "\u{9b}".as_bytes(),
    &[0xFF],
    &[0xE2, 0x82],
];

/// A seeded random stream of `pieces` dictionary tokens, numbers, printable
/// text and raw bytes.
pub fn random_stream(rng: &mut Rng, pieces: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for _ in 0..pieces {
        match rng.below(6) {
            0..=2 => out.extend_from_slice(DICTIONARY[rng.below(DICTIONARY.len())]),
            3 => {
                let number = rng.below(300);
                out.extend_from_slice(number.to_string().as_bytes());
                if rng.below(2) == 0 {
                    out.push(b';');
                } else {
                    out.push(b"mHJKABCDrsSTX@P"[rng.below(15)]);
                }
            }
            4 => {
                for _ in 0..rng.range_inclusive(1, 12) {
                    out.push(b"abcXYZ 0123456789.,-_|"[rng.below(22)]);
                }
            }
            _ => {
                for _ in 0..rng.range_inclusive(1, 3) {
                    out.push((rng.next_u64() >> 56) as u8);
                }
            }
        }
    }
    out
}
