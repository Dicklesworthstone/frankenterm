//! Canonical screen dumps and the golden files that hold them.
//!
//! A dump has two halves, so a reviewer can read each on its own:
//!
//! - **Text**: the cursor (1-based row,col and visibility), the screen mode
//!   (DECSCNM), then one line per visible row: `RR` plus a line-size marker
//!   (` ` normal, `W` double width, `T`/`B` double height top/bottom), then
//!   the row's characters between bars. Trailing blanks with default
//!   attributes are left out; a blank that carries attributes is kept.
//! - **Attributes**: one JSON object per row that has non-default cells:
//!   `{"row":R,"runs":[[col,cells,"attrs"],..]}`. A run is a maximal span of
//!   cells with the same attribute string; spans with default attributes are
//!   left out. Columns and rows are 1-based.
//!
//! The soft-wrap flag is left out on purpose: it is not visible, and which
//! edits clear it (EL, ICH, DCH at the right edge) differs between
//! terminals, so no reference fixes it. The differential harness still
//! compares it between engines, and the esctest cases assert it where
//! xterm's behavior is plain.
//!
//! A session's golden is `<session>.screens.txt` plus `<session>.attrs.json`,
//! with one section per recorded screen.

use std::fmt::Write as _;

use frankenterm_term::color::ColorAttribute;
use frankenterm_term::{CellAttributes, SemanticType};

use crate::differential::snapshot::{CellRun, EngineSnapshot, RowSnapshot};

#[derive(Clone, Debug, PartialEq)]
pub struct ScreenDump {
    pub text: Vec<String>,
    pub attrs: Vec<String>,
}

/// The visible screen of `snapshot`, which must hold at least `rows` rows.
pub fn dump(snapshot: &EngineSnapshot, rows: usize) -> ScreenDump {
    let cursor = &snapshot.cursor;
    let mut text = vec![
        format!(
            "cursor {},{} {}",
            cursor.y + 1,
            cursor.x + 1,
            format!("{:?}", cursor.visibility).to_lowercase()
        ),
        format!(
            "screen {}",
            if mode(snapshot, "reverse_video_mode") == Some("true") {
                "reverse"
            } else {
                "normal"
            }
        ),
    ];
    let mut attrs = Vec::new();
    let visible = &snapshot.rows[snapshot.rows.len() - rows..];
    for (index, row) in visible.iter().enumerate() {
        text.push(format!(
            "{:02}{}|{}|",
            index + 1,
            line_size_marker(row),
            row_text(row)
        ));
        if let Some(line) = attr_line(index + 1, row) {
            attrs.push(line);
        }
    }
    ScreenDump { text, attrs }
}

pub fn mode<'a>(snapshot: &'a EngineSnapshot, name: &str) -> Option<&'a str> {
    snapshot
        .modes
        .iter()
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.as_str())
}

fn line_size_marker(row: &RowSnapshot) -> char {
    // `RowSnapshot::line_bits` spells the bits out as `name=bool` pairs.
    if row.line_bits.contains("double_height_top=true") {
        'T'
    } else if row.line_bits.contains("double_height_bottom=true") {
        'B'
    } else if row.line_bits.contains("double_width=true") {
        'W'
    } else {
        ' '
    }
}

fn run_cells(run: &CellRun) -> usize {
    run.widths.iter().map(|&width| usize::from(width)).sum()
}

/// The row's characters; gaps between runs become spaces.
pub fn row_text(row: &RowSnapshot) -> String {
    let mut text = String::new();
    let mut column = 0;
    for run in &row.runs {
        while column < run.start {
            text.push(' ');
            column += 1;
        }
        text.push_str(&run.text);
        column = run.start + run_cells(run);
    }
    text
}

/// The canonical attribute string: tokens in a fixed order, empty for
/// default attributes. The cell's wrap bit is reported per row instead.
pub fn attr_string(attrs: &CellAttributes) -> String {
    let mut tokens: Vec<String> = Vec::new();
    let lower = |value: &dyn std::fmt::Debug| format!("{:?}", value).to_lowercase();
    let intensity = lower(&attrs.intensity());
    if intensity != "normal" {
        tokens.push(intensity);
    }
    if attrs.italic() {
        tokens.push("italic".to_string());
    }
    let underline = lower(&attrs.underline());
    if underline != "none" {
        tokens.push(format!("underline={}", underline));
    }
    let blink = lower(&attrs.blink());
    if blink != "none" {
        tokens.push(format!("blink={}", blink));
    }
    if attrs.reverse() {
        tokens.push("reverse".to_string());
    }
    if attrs.invisible() {
        tokens.push("invisible".to_string());
    }
    if attrs.strikethrough() {
        tokens.push("strike".to_string());
    }
    if attrs.overline() {
        tokens.push("overline".to_string());
    }
    let align = lower(&attrs.vertical_align());
    if align != "baseline" {
        tokens.push(format!("align={}", align));
    }
    for (name, color) in [
        ("fg", attrs.foreground()),
        ("bg", attrs.background()),
        ("ul", attrs.underline_color()),
    ] {
        if let Some(color) = color_string(color) {
            tokens.push(format!("{}={}", name, color));
        }
    }
    if let Some(link) = attrs.hyperlink() {
        let mut params: Vec<_> = link.params().iter().collect();
        params.sort();
        let mut token = format!("link={}", link.uri());
        for (key, value) in params {
            let _ = write!(token, ";{}={}", key, value);
        }
        tokens.push(token);
    }
    match attrs.semantic_type() {
        SemanticType::Output => {}
        SemanticType::Input => tokens.push("semantic=input".to_string()),
        SemanticType::Prompt => tokens.push("semantic=prompt".to_string()),
    }
    if let Some(images) = attrs.images() {
        if !images.is_empty() {
            tokens.push(format!("images={}", images.len()));
        }
    }
    tokens.join(" ")
}

fn color_string(color: ColorAttribute) -> Option<String> {
    match color {
        ColorAttribute::Default => None,
        ColorAttribute::PaletteIndex(index) => Some(index.to_string()),
        ColorAttribute::TrueColorWithDefaultFallback(rgb) => Some(rgb.to_rgb_string()),
        ColorAttribute::TrueColorWithPaletteFallback(rgb, index) => {
            Some(format!("{}/{}", rgb.to_rgb_string(), index))
        }
    }
}

fn attr_line(row_number: usize, row: &RowSnapshot) -> Option<String> {
    // Merge snapshot runs (split on any attribute difference, including the
    // per-cell wrap bit) by canonical string.
    let mut spans: Vec<(usize, usize, String)> = Vec::new();
    for run in &row.runs {
        let attrs = attr_string(&run.attrs);
        let cells = run_cells(run);
        match spans.last_mut() {
            Some(last) if last.2 == attrs && last.0 + last.1 == run.start => last.1 += cells,
            _ => spans.push((run.start, cells, attrs)),
        }
    }
    spans.retain(|span| !span.2.is_empty());
    if spans.is_empty() {
        return None;
    }
    let mut line = format!("{{\"row\":{},\"runs\":[", row_number);
    for (index, (start, cells, attrs)) in spans.iter().enumerate() {
        if index > 0 {
            line.push(',');
        }
        let _ = write!(line, "[{},{},{}]", start + 1, cells, json_string(attrs));
    }
    line.push_str("]}");
    Some(line)
}

pub fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// One section per screen; `keys` are the recorded keys, for the headers.
pub fn render_text_file(id_prefix: &str, keys: &[String], dumps: &[ScreenDump]) -> String {
    let mut out = format!(
        "# {} screen dumps (ft-yccm0.1.9): cursor, DECSCNM, then rows.\n\
         # Golden expectations: change only with a recorded reason (README).\n",
        id_prefix
    );
    for (index, (key, dump)) in keys.iter().zip(dumps).enumerate() {
        let _ = writeln!(out, "=== {:02} key {}", index, key);
        for line in &dump.text {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

pub fn render_attrs_file(session: &str, dumps: &[ScreenDump]) -> String {
    let mut out = format!("{{\"session\":{},\"screens\":[\n", json_string(session));
    for (index, dump) in dumps.iter().enumerate() {
        let _ = writeln!(out, "{{\"screen\":{},\"rows\":[", index);
        for (row, line) in dump.attrs.iter().enumerate() {
            out.push_str(line);
            if row + 1 < dump.attrs.len() {
                out.push(',');
            }
            out.push('\n');
        }
        out.push_str("]}");
        if index + 1 < dumps.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str("]}\n");
    out
}

/// The per-screen text sections of a `.screens.txt` golden.
pub fn parse_text_file(text: &str) -> Result<Vec<Vec<String>>, String> {
    let mut screens: Vec<Vec<String>> = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.starts_with('#') {
            continue;
        }
        if let Some(header) = line.strip_prefix("=== ") {
            let number: usize = header
                .split(' ')
                .next()
                .and_then(|word| word.parse().ok())
                .ok_or_else(|| format!("line {}: bad section header", index + 1))?;
            if number != screens.len() {
                return Err(format!(
                    "line {}: section {} out of order",
                    index + 1,
                    number
                ));
            }
            screens.push(Vec::new());
            continue;
        }
        screens
            .last_mut()
            .ok_or_else(|| format!("line {}: text before the first section", index + 1))?
            .push(line.to_string());
    }
    Ok(screens)
}

/// The per-screen row lines of an `.attrs.json` golden, in the exact shape
/// `render_attrs_file` writes.
pub fn parse_attrs_file(text: &str) -> Result<Vec<Vec<String>>, String> {
    let mut lines = text.lines().enumerate();
    match lines.next() {
        Some((_, first)) if first.starts_with("{\"session\":") && first.ends_with('[') => {}
        _ => return Err("line 1: expected the session header".to_string()),
    }
    let mut screens: Vec<Vec<String>> = Vec::new();
    let mut open = false;
    let mut closed = false;
    for (index, line) in lines {
        let line_no = index + 1;
        if closed {
            return Err(format!("line {}: text after the closing bracket", line_no));
        }
        if !open {
            if line == "]}" {
                closed = true;
                continue;
            }
            let header = format!("{{\"screen\":{},\"rows\":[", screens.len());
            if line != header {
                return Err(format!("line {}: expected {}", line_no, header));
            }
            screens.push(Vec::new());
            open = true;
            continue;
        }
        if line == "]}" || line == "]}," {
            open = false;
            continue;
        }
        let row = line.strip_suffix(',').unwrap_or(line);
        if !row.starts_with("{\"row\":") || !row.ends_with("]}") {
            return Err(format!("line {}: expected a row object", line_no));
        }
        screens
            .last_mut()
            .expect("a section is open")
            .push(row.to_string());
    }
    if open || !closed {
        return Err("unterminated attrs file".to_string());
    }
    Ok(screens)
}

/// `None` when equal; otherwise the differing lines, expected first.
pub fn compare(expected: &ScreenDump, actual: &ScreenDump) -> Option<String> {
    if expected == actual {
        return None;
    }
    let mut out = String::new();
    let mut shown = 0;
    let depth = expected.text.len().max(actual.text.len());
    for index in 0..depth {
        let want = expected.text.get(index);
        let got = actual.text.get(index);
        if want == got {
            continue;
        }
        shown += 1;
        if shown > 6 {
            out.push_str("  (more text lines differ)\n");
            break;
        }
        let _ = writeln!(
            out,
            "  expected {:?}",
            want.map(String::as_str).unwrap_or("")
        );
        let _ = writeln!(
            out,
            "  actual   {:?}",
            got.map(String::as_str).unwrap_or("")
        );
    }
    let missing: Vec<_> = expected
        .attrs
        .iter()
        .filter(|line| !actual.attrs.contains(line))
        .collect();
    let extra: Vec<_> = actual
        .attrs
        .iter()
        .filter(|line| !expected.attrs.contains(line))
        .collect();
    for line in missing.iter().take(4) {
        let _ = writeln!(out, "  attrs expected {}", line);
    }
    for line in extra.iter().take(4) {
        let _ = writeln!(out, "  attrs actual   {}", line);
    }
    if missing.len() > 4 || extra.len() > 4 {
        out.push_str("  (more attribute rows differ)\n");
    }
    if out.is_empty() {
        // Same lines in a different order.
        out.push_str("  attribute rows are out of order\n");
    }
    Some(out)
}
