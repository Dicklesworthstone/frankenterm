use std::sync::Arc;

use finl_unicode::grapheme_clusters::Graphemes;
use frankenterm_term::color::ColorPalette;
use frankenterm_term::{
    grapheme_column_width, Screen, Terminal, TerminalConfiguration, TerminalSize,
};

#[derive(Debug)]
struct TestConfig {
    scrollback: usize,
}

impl TerminalConfiguration for TestConfig {
    fn scrollback_size(&self) -> usize {
        self.scrollback
    }

    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }
}

fn make_term(rows: usize, cols: usize, scrollback: usize) -> Terminal {
    Terminal::new(
        TerminalSize {
            rows,
            cols,
            pixel_width: cols * 8,
            pixel_height: rows * 16,
            dpi: 96,
        },
        Arc::new(TestConfig { scrollback }),
        "WezTerm",
        "test",
        Box::new(Vec::new()),
    )
}

fn assert_cursor_mapping_consistent(term: &Terminal, rows: usize, cols: usize) {
    let cursor = term.cursor_pos();
    assert!(
        cursor.x <= cols,
        "cursor column must remain within or at right edge: x={} cols={}",
        cursor.x,
        cols
    );
    assert!(
        cursor.y >= 0 && (cursor.y as usize) < rows,
        "cursor row must remain in visible bounds: y={} rows={}",
        cursor.y,
        rows
    );

    let screen = term.screen();
    let phys_row = screen.phys_row(cursor.y);
    assert!(
        phys_row < screen.scrollback_rows(),
        "cursor physical row must index existing screen lines: phys_row={} line_count={}",
        phys_row,
        screen.scrollback_rows()
    );

    let stable_row = screen.visible_row_to_stable_row(cursor.y);
    let roundtrip = screen
        .stable_row_to_phys(stable_row)
        .expect("stable row for cursor should map back to a physical row");
    assert_eq!(
        roundtrip, phys_row,
        "cursor stable-row mapping should roundtrip through phys-row"
    );
}

fn is_zero_width_grapheme(g: &str) -> bool {
    grapheme_column_width(g, None) == 0
}

fn logical_lines_snapshot(screen: &Screen) -> Vec<String> {
    let mut logical_lines = Vec::new();
    let mut current = String::new();

    screen.for_each_phys_line(|_, line| {
        current.push_str(&line.as_str());
        if line.last_cell_was_wrapped() {
            return;
        }
        logical_lines.push(std::mem::take(&mut current));
    });

    if !current.is_empty() {
        logical_lines.push(current);
    }

    while logical_lines
        .last()
        .map(|line| line.is_empty())
        .unwrap_or(false)
    {
        logical_lines.pop();
    }

    logical_lines
}

fn assert_no_dangling_zero_width_boundaries(screen: &Screen) {
    let mut prior_row_wrapped = false;
    screen.for_each_phys_line(|phys_idx, line| {
        let text = line.as_str();
        if prior_row_wrapped {
            if let Some(first) = Graphemes::new(&text).next() {
                assert!(
                    !is_zero_width_grapheme(first),
                    "wrapped continuation starts with zero-width grapheme at phys_row={}: {:?}",
                    phys_idx,
                    text
                );
            }
        }

        if line.last_cell_was_wrapped() {
            if let Some(last) = Graphemes::new(&text).last() {
                assert!(
                    !is_zero_width_grapheme(last),
                    "wrapped segment ends with zero-width grapheme at phys_row={}: {:?}",
                    phys_idx,
                    text
                );
            }
        }

        prior_row_wrapped = line.last_cell_was_wrapped();
    });
}

#[test]
fn cursor_mapping_stays_consistent_during_resize_churn_with_typing() {
    let mut term = make_term(10, 8, 96);
    for idx in 0..120 {
        term.advance_bytes(format!("line{idx:03}-abcdefghijklmnop\r\n"));
    }

    let resize_steps = [
        (10usize, 6usize, 96u32),
        (10, 12, 96),
        (8, 5, 144),
        (12, 9, 96),
    ];
    for (step, (rows, cols, dpi)) in resize_steps.iter().copied().cycle().take(20).enumerate() {
        term.resize(TerminalSize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
            dpi,
        });
        term.advance_bytes(format!("typing-{step}\r\n"));
        assert_cursor_mapping_consistent(&term, rows, cols);
    }
}

#[test]
fn cursor_mapping_is_stable_across_noop_resize_repeats() {
    let mut term = make_term(8, 8, 64);
    for idx in 0..48 {
        term.advance_bytes(format!("payload-{idx:03}-abcdefghijklmno\r\n"));
    }

    let target_size = TerminalSize {
        rows: 8,
        cols: 6,
        pixel_width: 0,
        pixel_height: 0,
        dpi: 96,
    };
    term.resize(target_size);

    let baseline_cursor = term.cursor_pos();
    let baseline_stable = term.screen().visible_row_to_stable_row(baseline_cursor.y);
    assert_cursor_mapping_consistent(&term, target_size.rows, target_size.cols);

    for _ in 0..12 {
        term.resize(target_size);
        let cursor = term.cursor_pos();
        let stable = term.screen().visible_row_to_stable_row(cursor.y);

        assert_eq!(
            cursor.x, baseline_cursor.x,
            "noop resize should not move cursor column"
        );
        assert_eq!(
            cursor.y, baseline_cursor.y,
            "noop resize should not move cursor row"
        );
        assert_eq!(
            stable, baseline_stable,
            "noop resize should preserve stable-row mapping for cursor"
        );
        assert_cursor_mapping_consistent(&term, target_size.rows, target_size.cols);
    }
}

#[test]
fn unicode_logical_lines_remain_stable_across_resize_reflow_churn() {
    let mut term = make_term(10, 24, 160);
    let payloads = [
        "combining: cafe\u{0301} noe\u{0308}l",
        "emoji-mod: 👋🏿 👩‍💻",
        "emoji-zwj: 👨‍👩‍👧‍👦",
        "wide-cjk: 界界界界界",
        "mixed: A\u{0301}界🇺🇸🧪",
    ];

    for payload in payloads {
        term.advance_bytes(format!("{payload}\r\n"));
    }

    let baseline_logical = logical_lines_snapshot(term.screen());
    let baseline_compacted = baseline_logical.concat();
    assert!(
        baseline_logical.iter().any(|line| line.contains("👨‍👩‍👧‍👦")),
        "baseline snapshot should include emoji ZWJ sequence"
    );

    let resize_steps = [
        (10usize, 9usize, 96u32),
        (10, 14, 96),
        (10, 7, 144),
        (12, 11, 96),
        (8, 6, 96),
    ];

    for (rows, cols, dpi) in resize_steps.iter().copied().cycle().take(18) {
        term.resize(TerminalSize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
            dpi,
        });
        assert_cursor_mapping_consistent(&term, rows, cols);

        let logical = logical_lines_snapshot(term.screen());
        assert_eq!(
            logical.concat(),
            baseline_compacted,
            "Unicode payload bytes should stay stable across resize churn"
        );
        assert_no_dangling_zero_width_boundaries(term.screen());
    }
}

#[derive(Debug, Clone, Copy)]
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0
    }

    fn next_range(&mut self, start: usize, end_exclusive: usize) -> usize {
        assert!(start < end_exclusive);
        let span = (end_exclusive - start) as u64;
        let offset = (self.next_u64() % span) as usize;
        start + offset
    }
}

#[test]
fn unicode_corpus_remains_byte_stable_under_randomized_resize_sequences() {
    let mut term = make_term(12, 16, 512);
    let corpus = [
        "e\u{0301}cho",
        "👩‍💻 dev",
        "👨‍👩‍👧‍👦 family",
        "界界界",
        "👋🏿 wave",
        "🇺🇸🇯🇵 flags",
        "A\u{0308}B\u{0301}C",
        "क्‍ष",
        "🧪lab",
    ];

    let mut rng = Lcg::new(0x5eed_cafe_1234_9876);
    for idx in 0..96usize {
        let payload = corpus[rng.next_range(0, corpus.len())];
        term.advance_bytes(format!("{idx:03}:{payload}\r\n"));
    }

    let baseline_compacted = logical_lines_snapshot(term.screen()).concat();
    assert!(
        baseline_compacted.contains("👨‍👩‍👧‍👦"),
        "corpus baseline should include ZWJ family sequence"
    );
    assert!(
        baseline_compacted.contains("界"),
        "corpus baseline should include wide CJK characters"
    );

    for _ in 0..48 {
        let rows = rng.next_range(8, 14);
        let cols = rng.next_range(6, 19);
        let dpi = if rng.next_u64() & 1 == 0 { 96 } else { 144 };
        term.resize(TerminalSize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
            dpi,
        });

        assert_cursor_mapping_consistent(&term, rows, cols);
        assert_eq!(
            logical_lines_snapshot(term.screen()).concat(),
            baseline_compacted,
            "Unicode corpus bytes should remain stable under randomized resize sequence"
        );
        assert_no_dangling_zero_width_boundaries(term.screen());
    }
}
