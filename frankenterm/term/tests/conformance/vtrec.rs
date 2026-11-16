//! The `.vtrec` recording format written by
//! `fixtures/conformance/vttest/capture_vttest.py`.
//!
//! A recording is text. `#` lines are comments and `@` lines are directives;
//! `@screen NN` starts the bytes vttest wrote after one key. Every other line
//! is data: printable ASCII stands for itself, and `\e`, `\r`, `\n`, `\t`,
//! `\\` and `\xHH` stand for single bytes. Newlines in the file carry no data.

use std::path::Path;

pub struct Recording {
    pub session: String,
    pub rows: usize,
    pub cols: usize,
    pub screens: Vec<RecordedScreen>,
}

/// Every `*.vtrec` in `dir`, sorted by file name.
pub fn load_dir(dir: &Path) -> Result<Vec<Recording>, String> {
    let entries = std::fs::read_dir(dir).map_err(|err| format!("{}: {}", dir.display(), err))?;
    let mut paths = Vec::new();
    for entry in entries {
        let path = entry.map_err(|err| err.to_string())?.path();
        if path.extension().is_some_and(|ext| ext == "vtrec") {
            paths.push(path);
        }
    }
    paths.sort();
    paths
        .iter()
        .map(|path| {
            let text = std::fs::read_to_string(path)
                .map_err(|err| format!("{}: {}", path.display(), err))?;
            parse(&text).map_err(|err| format!("{}: {}", path.display(), err))
        })
        .collect()
}

pub struct RecordedScreen {
    /// The key vttest read before it wrote these bytes, as the driver
    /// printed it (a Python literal).
    pub key: String,
    pub bytes: Vec<u8>,
}

pub fn parse(text: &str) -> Result<Recording, String> {
    let mut session = None;
    let mut geometry = None;
    let mut screens: Vec<RecordedScreen> = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line_no = index + 1;
        if line.starts_with('#') {
            continue;
        }
        if let Some(directive) = line.strip_prefix('@') {
            let mut words = directive.split(' ');
            match words.next() {
                Some("session") => {
                    session = words.next().map(str::to_string);
                }
                Some("geometry") => {
                    // "@geometry 24x80.80 24x80 TERM=..": the second word is
                    // the screen size the pty had.
                    geometry = words.nth(1).and_then(parse_size);
                    if geometry.is_none() {
                        return Err(format!("line {}: bad @geometry", line_no));
                    }
                }
                Some("screen") => {
                    let number: usize = words
                        .next()
                        .and_then(|word| word.parse().ok())
                        .ok_or_else(|| format!("line {}: bad @screen", line_no))?;
                    if number != screens.len() {
                        return Err(format!(
                            "line {}: screen {} out of order (expected {})",
                            line_no,
                            number,
                            screens.len()
                        ));
                    }
                    let key = match (words.next(), directive.find(" key ")) {
                        (Some("key"), Some(at)) => directive[at + 5..].to_string(),
                        _ => return Err(format!("line {}: @screen without key", line_no)),
                    };
                    screens.push(RecordedScreen {
                        key,
                        bytes: Vec::new(),
                    });
                }
                Some("vttest") => {}
                other => return Err(format!("line {}: unknown directive {:?}", line_no, other)),
            }
            continue;
        }
        let screen = screens
            .last_mut()
            .ok_or_else(|| format!("line {}: data before the first @screen", line_no))?;
        unescape_into(line, &mut screen.bytes)
            .map_err(|err| format!("line {}: {}", line_no, err))?;
    }
    let session = session.ok_or("missing @session")?;
    let (rows, cols) = geometry.ok_or("missing @geometry")?;
    if screens.is_empty() {
        return Err("no screens".to_string());
    }
    Ok(Recording {
        session,
        rows,
        cols,
        screens,
    })
}

fn parse_size(word: &str) -> Option<(usize, usize)> {
    let (rows, cols) = word.split_once('x')?;
    Some((rows.parse().ok()?, cols.parse().ok()?))
}

/// Decodes one data line.
pub fn unescape_into(line: &str, out: &mut Vec<u8>) -> Result<(), String> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        if byte != b'\\' {
            if !(0x20..0x7f).contains(&byte) {
                return Err(format!("raw byte {:#04x} must be escaped", byte));
            }
            out.push(byte);
            i += 1;
            continue;
        }
        let (decoded, used) = match bytes.get(i + 1) {
            Some(b'e') => (0x1b, 2),
            Some(b'r') => (b'\r', 2),
            Some(b'n') => (b'\n', 2),
            Some(b't') => (b'\t', 2),
            Some(b'\\') => (b'\\', 2),
            Some(b'x') => {
                let hex = line
                    .get(i + 2..i + 4)
                    .ok_or("truncated \\x escape at the end of the line")?;
                let value =
                    u8::from_str_radix(hex, 16).map_err(|_| format!("bad \\x escape {:?}", hex))?;
                (value, 4)
            }
            other => return Err(format!("unknown escape \\{:?}", other.map(|&b| b as char))),
        };
        out.push(decoded);
        i += used;
    }
    Ok(())
}
