//! esctest-style assertion cases: feed bytes to a fresh engine, then assert
//! cursor, cells, modes, replies and clipboard writes. Expectations come from
//! the xterm control-sequence documentation and the DEC VT510 reference, not
//! from any engine's output. Rows and columns are 1-based, as in those
//! documents.

use std::io::Write;
use std::sync::{Arc, Mutex};

use frankenterm_term::{Clipboard, ClipboardSelection};

use super::dump::{attr_string, mode, row_text};
use crate::differential::engine::{Engine, EngineFactory, EngineIo, Geometry};
use crate::differential::snapshot::EngineSnapshot;

pub type Check = Result<(), String>;

pub struct EscCase {
    /// Unique within the suite; the runner prefixes `esctest/`.
    pub id: &'static str,
    /// Where the expectation comes from.
    pub reference: &'static str,
    pub rows: usize,
    pub cols: usize,
    pub run: fn(&mut Session) -> Check,
}

#[derive(Clone, Default)]
struct SharedBytes(Arc<Mutex<Vec<u8>>>);

impl Write for SharedBytes {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("reply buffer").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct RecordingClipboard {
    writes: Mutex<Vec<(ClipboardSelection, Option<String>)>>,
}

impl Clipboard for RecordingClipboard {
    fn set_contents(
        &self,
        selection: ClipboardSelection,
        data: Option<String>,
    ) -> anyhow::Result<()> {
        self.writes
            .lock()
            .expect("clipboard log")
            .push((selection, data));
        Ok(())
    }
}

/// One engine under test plus everything it writes back.
pub struct Session {
    engine: Box<dyn Engine>,
    rows: usize,
    cols: usize,
    replies: SharedBytes,
    clipboard: Arc<RecordingClipboard>,
}

impl Session {
    pub fn new(factory: &dyn EngineFactory, rows: usize, cols: usize) -> Session {
        let replies = SharedBytes::default();
        let clipboard = Arc::new(RecordingClipboard::default());
        let shared: Arc<dyn Clipboard> = clipboard.clone();
        let geometry = Geometry {
            rows,
            cols,
            scrollback: 64,
        };
        let engine = factory.build_with(
            &geometry,
            EngineIo {
                writer: Box::new(replies.clone()),
                clipboard: Some(shared),
            },
        );
        Session {
            engine,
            rows,
            cols,
            replies,
            clipboard,
        }
    }

    pub fn feed(&mut self, bytes: impl AsRef<[u8]>) {
        self.engine.feed(bytes.as_ref());
    }

    /// Writes `rows[i]` at the start of row `first + i`.
    pub fn fill(&mut self, first: usize, rows: &[&str]) {
        for (offset, text) in rows.iter().enumerate() {
            self.feed(format!("\x1b[{};1H{}", first + offset, text));
        }
    }

    pub fn snapshot(&self) -> EngineSnapshot {
        self.engine.snapshot()
    }

    fn visible_row(&self, snapshot: &EngineSnapshot, row: usize) -> Result<usize, String> {
        if row == 0 || row > self.rows {
            return Err(format!("row {} is off the {}-row screen", row, self.rows));
        }
        Ok(snapshot.rows.len() - self.rows + row - 1)
    }

    pub fn cursor(&self, row: usize, col: usize) -> Check {
        let cursor = self.snapshot().cursor;
        let actual = (cursor.y + 1, cursor.x + 1);
        if actual == (row as i64, col) {
            Ok(())
        } else {
            Err(format!(
                "cursor: expected {},{} got {},{}",
                row, col, actual.0, actual.1
            ))
        }
    }

    pub fn cursor_visible(&self, visible: bool) -> Check {
        let actual = format!("{:?}", self.snapshot().cursor.visibility);
        let expected = if visible { "Visible" } else { "Hidden" };
        if actual == expected {
            Ok(())
        } else {
            Err(format!(
                "cursor visibility: expected {} got {}",
                expected, actual
            ))
        }
    }

    /// Row text with trailing spaces ignored on both sides.
    pub fn row(&self, row: usize, expected: &str) -> Check {
        let snapshot = self.snapshot();
        let index = self.visible_row(&snapshot, row)?;
        let actual = row_text(&snapshot.rows[index]);
        if actual.trim_end() == expected.trim_end() {
            Ok(())
        } else {
            Err(format!(
                "row {}: expected {:?} got {:?}",
                row,
                expected.trim_end(),
                actual.trim_end()
            ))
        }
    }

    /// `rows[i]` is the expected text of row `first + i`.
    pub fn rows(&self, first: usize, rows: &[&str]) -> Check {
        for (offset, expected) in rows.iter().enumerate() {
            self.row(first + offset, expected)?;
        }
        Ok(())
    }

    /// The whole screen: rows past the end of `rows` must be blank.
    pub fn screen(&self, rows: &[&str]) -> Check {
        for row in 1..=self.rows {
            self.row(row, rows.get(row - 1).copied().unwrap_or(""))?;
        }
        Ok(())
    }

    pub fn wrapped(&self, row: usize, expected: bool) -> Check {
        let snapshot = self.snapshot();
        let index = self.visible_row(&snapshot, row)?;
        if snapshot.rows[index].wrapped == expected {
            Ok(())
        } else {
            Err(format!("row {} wrapped: expected {}", row, expected))
        }
    }

    /// The cell's canonical attribute string (see `dump::attr_string`).
    pub fn attrs(&self, row: usize, col: usize, expected: &str) -> Check {
        if col == 0 || col > self.cols {
            return Err(format!("column {} is off the screen", col));
        }
        let snapshot = self.snapshot();
        let index = self.visible_row(&snapshot, row)?;
        let actual = snapshot.rows[index]
            .runs
            .iter()
            .find(|run| {
                let cells: usize = run.widths.iter().map(|&w| usize::from(w)).sum();
                run.start < col && col <= run.start + cells
            })
            .map(|run| attr_string(&run.attrs))
            .unwrap_or_default();
        if actual == expected {
            Ok(())
        } else {
            Err(format!(
                "attrs at {},{}: expected {:?} got {:?}",
                row, col, expected, actual
            ))
        }
    }

    pub fn mode(&self, name: &str, expected: &str) -> Check {
        let snapshot = self.snapshot();
        match mode(&snapshot, name) {
            Some(actual) if actual == expected => Ok(()),
            Some(actual) => Err(format!(
                "mode {}: expected {} got {}",
                name, expected, actual
            )),
            None => Err(format!("mode {} is not in the snapshot", name)),
        }
    }

    /// Everything the terminal wrote back since the last call.
    pub fn take_replies(&mut self) -> Vec<u8> {
        self.engine.wait_for_replies();
        std::mem::take(&mut *self.replies.0.lock().expect("reply buffer"))
    }

    pub fn reply(&mut self, expected: &[u8]) -> Check {
        let actual = self.take_replies();
        if actual == expected {
            Ok(())
        } else {
            Err(format!(
                "reply: expected {} got {}",
                escape_bytes(expected),
                escape_bytes(&actual)
            ))
        }
    }

    /// The last OSC 52 write, as (selection, data).
    pub fn clipboard(&self, selection: ClipboardSelection, expected: Option<&str>) -> Check {
        let writes = self.clipboard.writes.lock().expect("clipboard log");
        match writes.last() {
            Some((actual_selection, data))
                if *actual_selection == selection && data.as_deref() == expected =>
            {
                Ok(())
            }
            last => Err(format!(
                "clipboard: expected {:?} {:?} got {:?}",
                selection, expected, last
            )),
        }
    }
}

pub fn escape_bytes(bytes: &[u8]) -> String {
    let mut out = String::new();
    for &byte in bytes {
        match byte {
            0x1b => out.push_str("\\e"),
            b'\\' => out.push_str("\\\\"),
            0x20..=0x7e => out.push(byte as char),
            _ => out.push_str(&format!("\\x{:02x}", byte)),
        }
    }
    out
}
