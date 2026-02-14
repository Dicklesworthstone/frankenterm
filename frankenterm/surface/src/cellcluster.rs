use crate::line::CellRef;
use alloc::borrow::Cow;
use frankenterm_bidi::{BidiContext, Direction, ParagraphDirectionHint};
use frankenterm_cell::CellAttributes;
use frankenterm_char_props::emoji::Presentation;

extern crate alloc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

/// A `CellCluster` is another representation of a Line.
/// A `Vec<CellCluster>` is produced by walking through the Cells in
/// a line and collecting succesive Cells with the same attributes
/// together into a `CellCluster` instance.  Additional metadata to
/// aid in font rendering is also collected.
#[derive(Debug, Clone)]
pub struct CellCluster {
    pub attrs: CellAttributes,
    pub text: String,
    pub width: usize,
    pub presentation: Presentation,
    pub direction: Direction,
    byte_to_cell_idx: Vec<usize>,
    byte_to_cell_width: Vec<u8>,
    pub first_cell_idx: usize,
}

impl CellCluster {
    /// Given a byte index into `self.text`, return the corresponding
    /// cell index in the originating line.
    pub fn byte_to_cell_idx(&self, byte_idx: usize) -> usize {
        if self.byte_to_cell_idx.is_empty() {
            self.first_cell_idx + byte_idx
        } else {
            self.byte_to_cell_idx[byte_idx]
        }
    }

    pub fn byte_to_cell_width(&self, byte_idx: usize) -> u8 {
        if self.byte_to_cell_width.is_empty() {
            1
        } else {
            self.byte_to_cell_width[byte_idx]
        }
    }

    /// Compute the list of CellClusters from a set of visible cells.
    /// The input is typically the result of calling `Line::visible_cells()`.
    pub fn make_cluster<'a>(
        hint: usize,
        iter: impl Iterator<Item = CellRef<'a>>,
        bidi_hint: Option<ParagraphDirectionHint>,
    ) -> Vec<CellCluster> {
        let mut last_cluster = None;
        let mut clusters = Vec::new();
        let mut whitespace_run = 0;
        let mut only_whitespace = false;

        for c in iter {
            let cell_idx = c.cell_index();
            let presentation = c.presentation();
            let cell_str = c.str();
            let normalized_attr = if c.attrs().wrapped() {
                let mut attr_storage = c.attrs().clone();
                attr_storage.set_wrapped(false);
                Cow::Owned(attr_storage)
            } else {
                Cow::Borrowed(c.attrs())
            };

            last_cluster = match last_cluster.take() {
                None => {
                    // Start new cluster
                    only_whitespace = cell_str == " ";
                    whitespace_run = if only_whitespace { 1 } else { 0 };
                    Some(CellCluster::new(
                        hint,
                        presentation,
                        normalized_attr.into_owned(),
                        cell_str,
                        cell_idx,
                        c.width(),
                    ))
                }
                Some(mut last) => {
                    if last.attrs != *normalized_attr || last.presentation != presentation {
                        // Flush pending cluster and start a new one
                        clusters.push(last);

                        only_whitespace = cell_str == " ";
                        whitespace_run = if only_whitespace { 1 } else { 0 };
                        Some(CellCluster::new(
                            hint,
                            presentation,
                            normalized_attr.into_owned(),
                            cell_str,
                            cell_idx,
                            c.width(),
                        ))
                    } else {
                        // Add to current cluster.

                        // Force cluster to break when we get a run of 2 whitespace
                        // characters following non-whitespace.
                        // This reduces the amount of shaping work for scenarios where
                        // the terminal is wide and a long series of short lines are printed;
                        // the shaper can cache the few variations of trailing whitespace
                        // and focus on shaping the shorter cluster sequences.
                        // Or:
                        // when bidi is disabled, force break on whitespace boundaries.
                        // This reduces shaping load in the case where is a line is
                        // updated continually, but only a portion of it changes
                        // (eg: progress counter).
                        let was_whitespace = whitespace_run > 0;
                        if cell_str == " " {
                            whitespace_run += 1;
                        } else {
                            whitespace_run = 0;
                            only_whitespace = false;
                        }

                        let force_break = (!only_whitespace && whitespace_run > 2)
                            || (!only_whitespace && bidi_hint.is_none() && was_whitespace);

                        if force_break {
                            clusters.push(last);

                            only_whitespace = cell_str == " ";
                            if whitespace_run > 0 {
                                whitespace_run = 1;
                            }
                            Some(CellCluster::new(
                                hint,
                                presentation,
                                normalized_attr.into_owned(),
                                cell_str,
                                cell_idx,
                                c.width(),
                            ))
                        } else {
                            last.add(cell_str, cell_idx, c.width());
                            Some(last)
                        }
                    }
                }
            };
        }

        if let Some(cluster) = last_cluster {
            // Don't forget to include any pending cluster on the final step!
            clusters.push(cluster);
        }

        if let Some(hint) = bidi_hint {
            let mut resolved_clusters = vec![];

            let mut context = BidiContext::new();
            for cluster in clusters {
                Self::resolve_bidi(&mut context, hint, cluster, &mut resolved_clusters);
            }

            resolved_clusters
        } else {
            clusters
        }
    }

    fn resolve_bidi(
        context: &mut BidiContext,
        hint: ParagraphDirectionHint,
        cluster: CellCluster,
        resolved: &mut Vec<Self>,
    ) {
        let mut paragraph = Vec::with_capacity(cluster.text.len());
        let mut codepoint_index_to_byte_idx = Vec::with_capacity(cluster.text.len());
        for (byte_idx, c) in cluster.text.char_indices() {
            codepoint_index_to_byte_idx.push(byte_idx);
            paragraph.push(c);
        }

        context.resolve_paragraph(&paragraph, hint);
        for run in context.reordered_runs(0..paragraph.len()) {
            let mut text = String::with_capacity(run.range.end - run.range.start);
            let mut byte_to_cell_idx = vec![];
            let mut byte_to_cell_width = vec![];
            let mut width = 0usize;
            let mut first_cell_idx = None;

            // Note: if we wanted the actual bidi-re-ordered
            // text we should iterate over run.indices here,
            // however, cluster.text will be fed into harfbuzz
            // and that requires the original logical order
            // for the text, so we look at run.range instead.
            for cp_idx in run.range.clone() {
                let cp = paragraph[cp_idx];
                text.push(cp);

                let original_byte = codepoint_index_to_byte_idx[cp_idx];
                let cell_width = cluster.byte_to_cell_width(original_byte);
                width += cell_width as usize;

                let cell_idx = cluster.byte_to_cell_idx(original_byte);
                if first_cell_idx.is_none() {
                    first_cell_idx.replace(cell_idx);
                }

                if !cluster.byte_to_cell_width.is_empty() {
                    for _ in 0..cp.len_utf8() {
                        byte_to_cell_width.push(cell_width);
                    }
                }

                if !cluster.byte_to_cell_idx.is_empty() {
                    for _ in 0..cp.len_utf8() {
                        byte_to_cell_idx.push(cell_idx);
                    }
                }
            }

            resolved.push(CellCluster {
                attrs: cluster.attrs.clone(),
                text,
                width,
                direction: run.direction,
                presentation: cluster.presentation,
                byte_to_cell_width,
                byte_to_cell_idx,
                first_cell_idx: first_cell_idx.unwrap(),
            });
        }
    }

    /// Start off a new cluster with some initial data
    fn new(
        hint: usize,
        presentation: Presentation,
        attrs: CellAttributes,
        text: &str,
        cell_idx: usize,
        width: usize,
    ) -> CellCluster {
        let mut idx = Vec::new();
        if text.len() > 1 {
            // Prefer to avoid pushing any index data; this saves
            // allocating any storage until we have any cells that
            // are multibyte
            for _ in 0..text.len() {
                idx.push(cell_idx);
            }
        }

        let mut byte_to_cell_width = Vec::new();
        if width > 1 {
            for _ in 0..text.len() {
                byte_to_cell_width.push(width as u8);
            }
        }
        let mut storage = String::with_capacity(hint);
        storage.push_str(text);

        CellCluster {
            attrs,
            width,
            text: storage,
            presentation,
            byte_to_cell_idx: idx,
            byte_to_cell_width,
            first_cell_idx: cell_idx,
            direction: Direction::LeftToRight,
        }
    }

    /// Add to this cluster
    fn add(&mut self, text: &str, cell_idx: usize, width: usize) {
        self.width += width;
        if !self.byte_to_cell_idx.is_empty() {
            // We had at least one multi-byte cell in the past
            for _ in 0..text.len() {
                self.byte_to_cell_idx.push(cell_idx);
            }
        } else if text.len() > 1 {
            // Extrapolate the indices so far
            for n in 0..self.text.len() {
                self.byte_to_cell_idx.push(n + self.first_cell_idx);
            }
            // Now add this new multi-byte cell text
            for _ in 0..text.len() {
                self.byte_to_cell_idx.push(cell_idx);
            }
        }

        if !self.byte_to_cell_width.is_empty() {
            // We had at least one double-wide cell in the past
            for _ in 0..text.len() {
                self.byte_to_cell_width.push(width as u8);
            }
        } else if width > 1 {
            // Extrapolate the widths so far; they must all be single width
            for _ in 0..self.text.len() {
                self.byte_to_cell_width.push(1);
            }
            // and add the current double width cell
            for _ in 0..text.len() {
                self.byte_to_cell_width.push(width as u8);
            }
        }
        self.text.push_str(text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;
    use frankenterm_bidi::Direction;
    use frankenterm_cell::CellAttributes;
    use frankenterm_char_props::emoji::Presentation;

    fn make_cluster(text: &str, cell_idx: usize, width: usize) -> CellCluster {
        CellCluster::new(
            64,
            Presentation::Text,
            CellAttributes::default(),
            text,
            cell_idx,
            width,
        )
    }

    // ── CellCluster::new ──────────────────────────────

    #[test]
    fn new_single_byte_text() {
        let c = make_cluster("a", 0, 1);
        assert_eq!(c.text, "a");
        assert_eq!(c.width, 1);
        assert_eq!(c.first_cell_idx, 0);
        assert_eq!(c.direction, Direction::LeftToRight);
        assert!(c.byte_to_cell_idx.is_empty());
        assert!(c.byte_to_cell_width.is_empty());
    }

    #[test]
    fn new_multi_byte_text_populates_byte_to_cell_idx() {
        // Multi-byte char like "é" (2 bytes in UTF-8)
        let c = make_cluster("é", 5, 1);
        assert_eq!(c.text, "é");
        assert_eq!(c.first_cell_idx, 5);
        // text.len() > 1, so byte_to_cell_idx should be populated
        assert_eq!(c.byte_to_cell_idx.len(), "é".len());
        for &idx in &c.byte_to_cell_idx {
            assert_eq!(idx, 5);
        }
    }

    #[test]
    fn new_double_width_populates_byte_to_cell_width() {
        let c = make_cluster("A", 0, 2);
        assert_eq!(c.width, 2);
        assert_eq!(c.byte_to_cell_width.len(), 1);
        assert_eq!(c.byte_to_cell_width[0], 2);
    }

    #[test]
    fn new_single_width_no_cell_width_map() {
        let c = make_cluster("x", 0, 1);
        assert!(c.byte_to_cell_width.is_empty());
    }

    // ── byte_to_cell_idx ──────────────────────────────

    #[test]
    fn byte_to_cell_idx_empty_map_returns_offset() {
        let c = make_cluster("a", 3, 1);
        // empty byte_to_cell_idx: returns first_cell_idx + byte_idx
        assert_eq!(c.byte_to_cell_idx(0), 3);
    }

    #[test]
    fn byte_to_cell_idx_populated_map() {
        let c = make_cluster("é", 10, 1);
        // "é" is 2 bytes, map populated
        assert_eq!(c.byte_to_cell_idx(0), 10);
        assert_eq!(c.byte_to_cell_idx(1), 10);
    }

    // ── byte_to_cell_width ────────────────────────────

    #[test]
    fn byte_to_cell_width_empty_map_returns_one() {
        let c = make_cluster("a", 0, 1);
        assert_eq!(c.byte_to_cell_width(0), 1);
    }

    #[test]
    fn byte_to_cell_width_populated_map() {
        let c = make_cluster("X", 0, 2);
        assert_eq!(c.byte_to_cell_width(0), 2);
    }

    // ── CellCluster::add ──────────────────────────────

    #[test]
    fn add_single_byte_to_single_byte() {
        let mut c = make_cluster("a", 0, 1);
        c.add("b", 1, 1);
        assert_eq!(c.text, "ab");
        assert_eq!(c.width, 2);
        // Both single byte, no idx map
        assert!(c.byte_to_cell_idx.is_empty());
    }

    #[test]
    fn add_multi_byte_to_single_byte_extrapolates() {
        let mut c = make_cluster("a", 0, 1);
        c.add("é", 1, 1);
        assert_eq!(c.text, "aé");
        // After adding multi-byte, idx map should be extrapolated
        assert_eq!(c.byte_to_cell_idx.len(), "aé".len());
        assert_eq!(c.byte_to_cell_idx[0], 0); // "a" -> cell 0
                                              // "é" bytes -> cell 1
        for i in 1..c.byte_to_cell_idx.len() {
            assert_eq!(c.byte_to_cell_idx[i], 1);
        }
    }

    #[test]
    fn add_to_multi_byte_extends_map() {
        let mut c = make_cluster("é", 0, 1);
        c.add("x", 1, 1);
        assert_eq!(c.text, "éx");
        // Map was already populated from multi-byte start
        assert_eq!(c.byte_to_cell_idx.len(), "éx".len());
    }

    #[test]
    fn add_double_width_to_single_width_extrapolates_widths() {
        let mut c = make_cluster("a", 0, 1);
        c.add("W", 1, 2);
        assert_eq!(c.width, 3);
        assert_eq!(c.byte_to_cell_width.len(), "aW".len());
        assert_eq!(c.byte_to_cell_width[0], 1);
        assert_eq!(c.byte_to_cell_width[1], 2);
    }

    #[test]
    fn add_updates_total_width() {
        let mut c = make_cluster("a", 0, 1);
        c.add("b", 1, 1);
        c.add("c", 2, 1);
        assert_eq!(c.width, 3);
        assert_eq!(c.text, "abc");
    }

    // ── Debug / Clone ─────────────────────────────────

    #[test]
    fn cluster_debug() {
        let c = make_cluster("test", 0, 1);
        let dbg = format!("{:?}", c);
        assert!(dbg.contains("CellCluster"));
        assert!(dbg.contains("test"));
    }

    #[test]
    fn cluster_clone() {
        let c = make_cluster("hello", 0, 1);
        let cloned = c.clone();
        assert_eq!(c.text, cloned.text);
        assert_eq!(c.width, cloned.width);
        assert_eq!(c.first_cell_idx, cloned.first_cell_idx);
    }

    // ── Presentation / Direction ──────────────────────

    #[test]
    fn new_sets_text_presentation() {
        let c = CellCluster::new(
            64,
            Presentation::Emoji,
            CellAttributes::default(),
            "😀",
            0,
            2,
        );
        assert_eq!(c.presentation, Presentation::Emoji);
    }

    #[test]
    fn new_sets_ltr_direction() {
        let c = make_cluster("a", 0, 1);
        assert_eq!(c.direction, Direction::LeftToRight);
    }
}
