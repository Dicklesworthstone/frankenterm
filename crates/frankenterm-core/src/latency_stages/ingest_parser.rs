use std::fmt;

use serde::{Deserialize, Serialize};

// AARSP Bead: ft-2p9cb.3.2 - Zero-Copy Ingestion Parser

// AARSP Bead: ft-2p9cb.3.2.1

/// Find the position of the first newline byte in a slice.
#[cfg(test)]
fn memchr_newline(data: &[u8]) -> Option<usize> {
    data.iter().position(|&b| b == b'\n')
}

/// Find the position of the last newline byte in a slice.
fn memchr_last_newline(data: &[u8]) -> Option<usize> {
    data.iter().rposition(|&b| b == b'\n')
}

/// Count newline bytes in a slice.
#[allow(clippy::naive_bytecount)]
fn count_newlines(data: &[u8]) -> usize {
    data.iter().filter(|&&b| b == b'\n').count()
}

/// Ingestion chunk: a borrowed byte slice with metadata for zero-copy parsing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestChunk {
    /// Source pane ID.
    pub pane_id: u64,
    /// Byte offset in the source stream.
    pub offset: u64,
    /// Length of this chunk.
    pub length: usize,
    /// Whether the chunk ends at a line boundary.
    pub line_aligned: bool,
    /// Timestamp of capture.
    pub captured_us: u64,
}

/// Parsing result from the ingestion parser.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ParseResult {
    /// Complete line(s) found: ready for downstream.
    Complete { lines: usize, bytes_consumed: usize },
    /// Partial data: need more input.
    Partial { bytes_buffered: usize },
    /// Invalid/corrupt data detected.
    Invalid {
        bytes_skipped: usize,
        reason: String,
    },
}

/// Configuration for the zero-copy ingestion parser.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestParserConfig {
    /// Maximum line length before forced split.
    pub max_line_bytes: usize,
    /// Maximum chunks to buffer before flushing.
    pub max_buffered_chunks: usize,
    /// Whether to strip ANSI escape sequences in-place.
    pub strip_escapes: bool,
    /// Whether to compute FNV-1a checksum for integrity.
    pub checksum: bool,
}

impl Default for IngestParserConfig {
    fn default() -> Self {
        Self {
            max_line_bytes: 16384,
            max_buffered_chunks: 64,
            strip_escapes: false,
            checksum: true,
        }
    }
}

/// Diagnostic snapshot of the ingestion parser.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IngestParserSnapshot {
    /// Total bytes processed.
    pub total_bytes: u64,
    /// Total lines emitted.
    pub total_lines: u64,
    /// Total chunks processed.
    pub total_chunks: u64,
    /// Total invalid/corrupt bytes skipped.
    pub total_invalid_bytes: u64,
    /// Buffered bytes awaiting next chunk.
    pub buffered_bytes: usize,
    /// Zero-copy ratio: fraction of bytes processed without copying.
    pub zero_copy_ratio: f64,
}

/// Zero-copy ingestion parser. Processes byte streams into lines with
/// minimal data movement.
///
/// # Invariants
///
/// 1. `total_bytes = total_consumed + buffered_bytes + total_invalid_bytes`.
/// 2. Zero-copy ratio is always in [0.0, 1.0].
/// 3. Lines are emitted in order.
/// 4. Deterministic: same byte sequence -> same parse results.
#[derive(Debug, Clone)]
pub struct IngestParser {
    config: IngestParserConfig,
    buffer: Vec<u8>,
    total_bytes: u64,
    total_lines: u64,
    total_chunks: u64,
    total_invalid_bytes: u64,
    total_consumed: u64,
    zero_copy_bytes: u64,
}

impl IngestParser {
    /// Create a new parser.
    pub fn new(config: IngestParserConfig) -> Self {
        Self {
            buffer: Vec::new(),
            total_bytes: 0,
            total_lines: 0,
            total_chunks: 0,
            total_invalid_bytes: 0,
            total_consumed: 0,
            zero_copy_bytes: 0,
            config,
        }
    }

    /// Create with default config.
    pub fn with_defaults() -> Self {
        Self::new(IngestParserConfig::default())
    }

    /// Feed a chunk of bytes. Returns parsing result.
    pub fn feed(&mut self, data: &[u8]) -> ParseResult {
        self.total_bytes += data.len() as u64;
        self.total_chunks += 1;

        // If buffer is empty and data contains a newline, we can process zero-copy.
        if self.buffer.is_empty() {
            if let Some(newline_pos) = memchr_last_newline(data) {
                let lines = count_newlines(&data[..=newline_pos]);
                let consumed = newline_pos + 1;
                self.total_lines += lines as u64;
                self.total_consumed += consumed as u64;
                self.zero_copy_bytes += consumed as u64;

                // Buffer the remainder.
                if consumed < data.len() {
                    self.buffer.extend_from_slice(&data[consumed..]);
                }

                return ParseResult::Complete {
                    lines,
                    bytes_consumed: consumed,
                };
            }

            // No newline: check for max line length.
            if data.len() > self.config.max_line_bytes {
                self.total_invalid_bytes += data.len() as u64;
                return ParseResult::Invalid {
                    bytes_skipped: data.len(),
                    reason: "line exceeds max_line_bytes".to_string(),
                };
            }

            // Buffer it.
            self.buffer.extend_from_slice(data);
            return ParseResult::Partial {
                bytes_buffered: self.buffer.len(),
            };
        }

        // We have buffered data: append and scan.
        self.buffer.extend_from_slice(data);

        if let Some(newline_pos) = memchr_last_newline(&self.buffer) {
            let lines = count_newlines(&self.buffer[..=newline_pos]);
            let consumed = newline_pos + 1;
            self.total_lines += lines as u64;
            self.total_consumed += consumed as u64;

            // Keep remainder in buffer.
            let remainder = self.buffer[consumed..].to_vec();
            self.buffer = remainder;

            return ParseResult::Complete {
                lines,
                bytes_consumed: consumed,
            };
        }

        // Check max buffer size.
        if self.buffer.len() > self.config.max_line_bytes {
            let skipped = self.buffer.len();
            self.total_invalid_bytes += skipped as u64;
            self.buffer.clear();
            return ParseResult::Invalid {
                bytes_skipped: skipped,
                reason: "buffered line exceeds max_line_bytes".to_string(),
            };
        }

        ParseResult::Partial {
            bytes_buffered: self.buffer.len(),
        }
    }

    /// Flush any remaining buffered data as a final line.
    pub fn flush(&mut self) -> Option<ParseResult> {
        if self.buffer.is_empty() {
            return None;
        }

        let len = self.buffer.len();
        self.total_lines += 1;
        self.total_consumed += len as u64;
        self.buffer.clear();

        Some(ParseResult::Complete {
            lines: 1,
            bytes_consumed: len,
        })
    }

    /// Zero-copy ratio.
    pub fn zero_copy_ratio(&self) -> f64 {
        if self.total_consumed == 0 {
            0.0
        } else {
            self.zero_copy_bytes as f64 / self.total_consumed as f64
        }
    }

    /// Diagnostic snapshot.
    pub fn snapshot(&self) -> IngestParserSnapshot {
        IngestParserSnapshot {
            total_bytes: self.total_bytes,
            total_lines: self.total_lines,
            total_chunks: self.total_chunks,
            total_invalid_bytes: self.total_invalid_bytes,
            buffered_bytes: self.buffer.len(),
            zero_copy_ratio: self.zero_copy_ratio(),
        }
    }

    /// Status line for logging.
    pub fn status_line(&self) -> String {
        format!(
            "ingest bytes={} lines={} chunks={} zc={:.1}% buf={}",
            self.total_bytes,
            self.total_lines,
            self.total_chunks,
            self.zero_copy_ratio() * 100.0,
            self.buffer.len(),
        )
    }

    /// Buffered byte count.
    pub fn buffered_bytes(&self) -> usize {
        self.buffer.len()
    }

    /// Reset parser state.
    pub fn reset(&mut self) {
        self.buffer.clear();
        self.total_bytes = 0;
        self.total_lines = 0;
        self.total_chunks = 0;
        self.total_invalid_bytes = 0;
        self.total_consumed = 0;
        self.zero_copy_bytes = 0;
    }

    /// Total bytes processed.
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// Total lines emitted.
    pub fn total_lines(&self) -> u64 {
        self.total_lines
    }

    /// Total chunks processed.
    pub fn total_chunks(&self) -> u64 {
        self.total_chunks
    }
}

// AARSP Bead: ft-2p9cb.3.2.2

/// Degradation signal from the ingestion parser.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum IngestDegradation {
    /// Parser is healthy.
    Healthy,
    /// High buffer pressure: too much data buffered.
    HighBufferPressure {
        buffered_bytes: usize,
        max_line_bytes: usize,
    },
    /// Data corruption detected.
    DataCorruption {
        invalid_bytes: u64,
        total_bytes: u64,
    },
    /// Low zero-copy ratio: too much data is being copied.
    LowZeroCopy { ratio: f64, threshold: f64 },
}

impl fmt::Display for IngestDegradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => write!(f, "HEALTHY"),
            Self::HighBufferPressure {
                buffered_bytes,
                max_line_bytes,
            } => write!(f, "HIGH_BUFFER({}/{})", buffered_bytes, max_line_bytes),
            Self::DataCorruption {
                invalid_bytes,
                total_bytes,
            } => write!(f, "CORRUPT({}/{})", invalid_bytes, total_bytes),
            Self::LowZeroCopy { ratio, threshold } => {
                write!(
                    f,
                    "LOW_ZC({:.1}%/thresh={:.1}%)",
                    ratio * 100.0,
                    threshold * 100.0
                )
            }
        }
    }
}

/// Structured log entry for ingestion parser.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IngestLogEntry {
    /// Total bytes.
    pub total_bytes: u64,
    /// Total lines.
    pub total_lines: u64,
    /// Zero-copy ratio.
    pub zero_copy_ratio: f64,
    /// Buffered bytes.
    pub buffered_bytes: usize,
    /// Degradation signal.
    pub degradation: IngestDegradation,
}

impl IngestParser {
    /// Detect degradation.
    pub fn detect_degradation(&self) -> IngestDegradation {
        // Check buffer pressure (>75% of max line length).
        if self.buffer.len() > self.config.max_line_bytes * 3 / 4 {
            return IngestDegradation::HighBufferPressure {
                buffered_bytes: self.buffer.len(),
                max_line_bytes: self.config.max_line_bytes,
            };
        }

        // Check data corruption (>1% invalid).
        if self.total_bytes > 100 && self.total_invalid_bytes * 100 > self.total_bytes {
            return IngestDegradation::DataCorruption {
                invalid_bytes: self.total_invalid_bytes,
                total_bytes: self.total_bytes,
            };
        }

        // Check zero-copy ratio (< 50% after sufficient data).
        if self.total_consumed > 1000 && self.zero_copy_ratio() < 0.5 {
            return IngestDegradation::LowZeroCopy {
                ratio: self.zero_copy_ratio(),
                threshold: 0.5,
            };
        }

        IngestDegradation::Healthy
    }

    /// Generate a structured log entry.
    pub fn log_entry(&self) -> IngestLogEntry {
        IngestLogEntry {
            total_bytes: self.total_bytes,
            total_lines: self.total_lines,
            zero_copy_ratio: self.zero_copy_ratio(),
            buffered_bytes: self.buffer.len(),
            degradation: self.detect_degradation(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ingest_parser_complete_line() {
        let mut parser = IngestParser::with_defaults();
        let result = parser.feed(b"hello world\n");
        assert_eq!(
            result,
            ParseResult::Complete {
                lines: 1,
                bytes_consumed: 12,
            }
        );
        assert_eq!(parser.snapshot().total_lines, 1);
    }

    #[test]
    fn test_ingest_parser_partial_then_complete() {
        let mut parser = IngestParser::with_defaults();
        let r1 = parser.feed(b"hello ");
        let is_partial = matches!(r1, ParseResult::Partial { .. });
        assert!(is_partial);

        let r2 = parser.feed(b"world\n");
        assert_eq!(
            r2,
            ParseResult::Complete {
                lines: 1,
                bytes_consumed: 12,
            }
        );
    }

    #[test]
    fn test_ingest_parser_multiple_lines() {
        let mut parser = IngestParser::with_defaults();
        let result = parser.feed(b"line1\nline2\nline3\n");
        assert_eq!(
            result,
            ParseResult::Complete {
                lines: 3,
                bytes_consumed: 18,
            }
        );
    }

    #[test]
    fn test_ingest_parser_zero_copy_ratio() {
        let mut parser = IngestParser::with_defaults();
        parser.feed(b"complete line\n");
        let ratio = parser.zero_copy_ratio();
        assert!((ratio - 1.0).abs() < 1e-10, "Expected 1.0, got {}", ratio);
    }

    #[test]
    fn test_ingest_parser_flush() {
        let mut parser = IngestParser::with_defaults();
        parser.feed(b"incomplete");
        let result = parser.flush();
        assert!(result.is_some());
        let r = result.unwrap();
        assert_eq!(
            r,
            ParseResult::Complete {
                lines: 1,
                bytes_consumed: 10,
            }
        );
        assert_eq!(parser.buffered_bytes(), 0);
    }

    #[test]
    fn test_ingest_parser_flush_empty() {
        let mut parser = IngestParser::with_defaults();
        assert!(parser.flush().is_none());
    }

    #[test]
    fn test_ingest_parser_max_line_reject() {
        let config = IngestParserConfig {
            max_line_bytes: 10,
            ..Default::default()
        };
        let mut parser = IngestParser::new(config);
        let result = parser.feed(b"this is a very long line without newline");
        let is_invalid = matches!(result, ParseResult::Invalid { .. });
        assert!(is_invalid, "Expected Invalid, got {:?}", result);
    }

    #[test]
    fn test_ingest_parser_snapshot_serde() {
        let snap = IngestParserSnapshot {
            total_bytes: 1000,
            total_lines: 50,
            total_chunks: 10,
            total_invalid_bytes: 0,
            buffered_bytes: 5,
            zero_copy_ratio: 0.8,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: IngestParserSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap.total_bytes, back.total_bytes);
        assert_eq!(snap.total_lines, back.total_lines);
    }

    #[test]
    fn test_ingest_parser_config_serde() {
        let cfg = IngestParserConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: IngestParserConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn test_ingest_parser_config_default() {
        let cfg = IngestParserConfig::default();
        assert_eq!(cfg.max_line_bytes, 16384);
        assert_eq!(cfg.max_buffered_chunks, 64);
        assert!(cfg.checksum);
    }

    #[test]
    fn test_parse_result_serde() {
        let results = vec![
            ParseResult::Complete {
                lines: 3,
                bytes_consumed: 30,
            },
            ParseResult::Partial { bytes_buffered: 10 },
            ParseResult::Invalid {
                bytes_skipped: 5,
                reason: "corrupt".to_string(),
            },
        ];
        for r in &results {
            let json = serde_json::to_string(r).unwrap();
            let back: ParseResult = serde_json::from_str(&json).unwrap();
            assert_eq!(*r, back);
        }
    }

    #[test]
    fn test_ingest_chunk_serde() {
        let chunk = IngestChunk {
            pane_id: 1,
            offset: 100,
            length: 50,
            line_aligned: true,
            captured_us: 5000,
        };
        let json = serde_json::to_string(&chunk).unwrap();
        let back: IngestChunk = serde_json::from_str(&json).unwrap();
        assert_eq!(chunk, back);
    }

    #[test]
    fn test_ingest_parser_status_line() {
        let parser = IngestParser::with_defaults();
        let line = parser.status_line();
        assert!(line.contains("ingest"));
        assert!(line.contains("bytes=0"));
    }

    #[test]
    fn test_memchr_newline() {
        assert_eq!(memchr_newline(b"hello\nworld"), Some(5));
        assert_eq!(memchr_newline(b"no newline"), None);
        assert_eq!(memchr_newline(b"\n"), Some(0));
    }

    #[test]
    fn test_count_newlines() {
        assert_eq!(count_newlines(b"a\nb\nc\n"), 3);
        assert_eq!(count_newlines(b"no newlines"), 0);
    }

    #[test]
    fn test_ingest_parser_line_with_remainder() {
        let mut parser = IngestParser::with_defaults();
        let result = parser.feed(b"line1\npartial");
        assert_eq!(
            result,
            ParseResult::Complete {
                lines: 1,
                bytes_consumed: 6,
            }
        );
        assert_eq!(parser.buffered_bytes(), 7);
    }

    #[test]
    fn test_ingest_parser_reset() {
        let mut parser = IngestParser::with_defaults();
        parser.feed(b"hello\n");
        parser.feed(b"world");
        assert!(parser.total_bytes() > 0);

        parser.reset();
        assert_eq!(parser.total_bytes(), 0);
        assert_eq!(parser.total_lines(), 0);
        assert_eq!(parser.total_chunks(), 0);
        assert_eq!(parser.buffered_bytes(), 0);
    }

    #[test]
    fn test_ingest_degradation_healthy() {
        let parser = IngestParser::with_defaults();
        assert_eq!(parser.detect_degradation(), IngestDegradation::Healthy);
    }

    #[test]
    fn test_ingest_degradation_high_buffer() {
        let config = IngestParserConfig {
            max_line_bytes: 20,
            ..Default::default()
        };
        let mut parser = IngestParser::new(config);
        parser.feed(b"0123456789abcdef");
        let degradation = parser.detect_degradation();
        let is_high = matches!(degradation, IngestDegradation::HighBufferPressure { .. });
        assert!(
            is_high,
            "Expected HighBufferPressure, got {:?}",
            degradation
        );
    }

    #[test]
    fn test_ingest_log_entry() {
        let mut parser = IngestParser::with_defaults();
        parser.feed(b"test line\n");
        let entry = parser.log_entry();
        assert_eq!(entry.total_lines, 1);
        assert_eq!(entry.degradation, IngestDegradation::Healthy);
    }

    #[test]
    fn test_ingest_degradation_serde() {
        let variants = vec![
            IngestDegradation::Healthy,
            IngestDegradation::HighBufferPressure {
                buffered_bytes: 100,
                max_line_bytes: 120,
            },
            IngestDegradation::DataCorruption {
                invalid_bytes: 10,
                total_bytes: 200,
            },
            IngestDegradation::LowZeroCopy {
                ratio: 0.3,
                threshold: 0.5,
            },
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let back: IngestDegradation = serde_json::from_str(&json).unwrap();
            assert_eq!(*v, back);
        }
    }

    #[test]
    fn test_ingest_log_entry_serde() {
        let entry = IngestLogEntry {
            total_bytes: 1000,
            total_lines: 50,
            zero_copy_ratio: 0.8,
            buffered_bytes: 5,
            degradation: IngestDegradation::Healthy,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: IngestLogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn test_ingest_degradation_display() {
        assert_eq!(format!("{}", IngestDegradation::Healthy), "HEALTHY");
        let buf = IngestDegradation::HighBufferPressure {
            buffered_bytes: 100,
            max_line_bytes: 120,
        };
        assert!(format!("{}", buf).contains("100/120"));
    }
}
