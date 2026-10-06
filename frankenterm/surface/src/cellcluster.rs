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
    fn normalize_cell_width(width: usize) -> u8 {
        width.clamp(1, 2) as u8
    }

    /// Given a byte index into `self.text`, return the corresponding
    /// cell index in the originating line.
    pub fn byte_to_cell_idx(&self, byte_idx: usize) -> usize {
        if self.byte_to_cell_idx.is_empty() {
            self.first_cell_idx.saturating_add(byte_idx)
        } else {
            self.byte_to_cell_idx
                .get(byte_idx)
                .copied()
                .or_else(|| self.byte_to_cell_idx.last().copied())
                .unwrap_or(self.first_cell_idx)
        }
    }

    pub fn byte_to_cell_width(&self, byte_idx: usize) -> u8 {
        if self.byte_to_cell_width.is_empty() {
            1
        } else {
            self.byte_to_cell_width
                .get(byte_idx)
                .copied()
                .or_else(|| self.byte_to_cell_width.last().copied())
                .unwrap_or(1)
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
                first_cell_idx: first_cell_idx.unwrap_or(0),
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
        let width = Self::normalize_cell_width(width);
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
                byte_to_cell_width.push(width);
            }
        }
        let mut storage = String::with_capacity(hint);
        storage.push_str(text);

        CellCluster {
            attrs,
            width: usize::from(width),
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
        let width = Self::normalize_cell_width(width);
        self.width = self.width.saturating_add(usize::from(width));
        if !self.byte_to_cell_idx.is_empty() {
            // We had at least one multi-byte cell in the past
            for _ in 0..text.len() {
                self.byte_to_cell_idx.push(cell_idx);
            }
        } else if text.len() > 1 {
            // Extrapolate the indices so far
            for n in 0..self.text.len() {
                self.byte_to_cell_idx
                    .push(self.first_cell_idx.saturating_add(n));
            }
            // Now add this new multi-byte cell text
            for _ in 0..text.len() {
                self.byte_to_cell_idx.push(cell_idx);
            }
        }

        if !self.byte_to_cell_width.is_empty() {
            // We had at least one double-wide cell in the past
            for _ in 0..text.len() {
                self.byte_to_cell_width.push(width);
            }
        } else if width > 1 {
            // Extrapolate the widths so far; they must all be single width
            for _ in 0..self.text.len() {
                self.byte_to_cell_width.push(1);
            }
            // and add the current double width cell
            for _ in 0..text.len() {
                self.byte_to_cell_width.push(width);
            }
        }
        self.text.push_str(text);
    }

    /// Joins clusters that sit side by side in one line into a single run,
    /// so the GUI can shape text whose cells differ only in paint attributes
    /// (colors, underline, hyperlinks) once (ft-yccm0.4.3.4). The result
    /// carries the first part's attributes, and byte-to-cell maps that are
    /// exact over the joined text. `None` when the parts are empty, do not
    /// each start where the previous one ends, or differ in presentation or
    /// direction.
    pub fn concat(parts: &[&CellCluster]) -> Option<CellCluster> {
        let (first, rest) = parts.split_first()?;
        let mut next_cell = first.first_cell_idx.checked_add(first.width)?;
        for part in rest {
            if part.first_cell_idx != next_cell
                || part.presentation != first.presentation
                || part.direction != first.direction
            {
                return None;
            }
            next_cell = part.first_cell_idx.checked_add(part.width)?;
        }
        let text_len = parts.iter().map(|part| part.text.len()).sum();
        // Empty maps mean "one byte per single-width cell"; the joined maps
        // stay empty only when every part's are.
        let explicit_idx = parts.iter().any(|part| !part.byte_to_cell_idx.is_empty());
        let explicit_width = parts.iter().any(|part| !part.byte_to_cell_width.is_empty());
        let mut joined = CellCluster {
            attrs: first.attrs.clone(),
            text: String::with_capacity(text_len),
            width: next_cell - first.first_cell_idx,
            presentation: first.presentation,
            direction: first.direction,
            byte_to_cell_idx: Vec::with_capacity(if explicit_idx { text_len } else { 0 }),
            byte_to_cell_width: Vec::with_capacity(if explicit_width { text_len } else { 0 }),
            first_cell_idx: first.first_cell_idx,
        };
        for part in parts {
            for byte_idx in 0..part.text.len() {
                if explicit_idx {
                    joined
                        .byte_to_cell_idx
                        .push(part.byte_to_cell_idx(byte_idx));
                }
                if explicit_width {
                    joined
                        .byte_to_cell_width
                        .push(part.byte_to_cell_width(byte_idx));
                }
            }
            joined.text.push_str(&part.text);
        }
        Some(joined)
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

    // ── CellCluster::concat (ft-yccm0.4.3.4) ──────────

    #[test]
    fn concat_joins_adjacent_clusters_with_exact_cell_maps() {
        let mut ab = make_cluster("a", 0, 1);
        ab.add("b", 1, 1);
        ab.attrs
            .set_foreground(frankenterm_cell::color::ColorAttribute::PaletteIndex(1));
        let mut cd = make_cluster("c", 2, 1);
        cd.add("d", 3, 1);

        // Single-byte cells keep the compact empty maps.
        let joined = CellCluster::concat(&[&ab, &cd]).unwrap();
        assert_eq!(joined.text, "abcd");
        assert_eq!((joined.first_cell_idx, joined.width), (0, 4));
        assert!(joined.byte_to_cell_idx.is_empty() && joined.byte_to_cell_width.is_empty());
        let cells: Vec<usize> = (0..4).map(|byte| joined.byte_to_cell_idx(byte)).collect();
        assert_eq!(cells, vec![0, 1, 2, 3]);
        // The first part's attributes carry over.
        assert_eq!(joined.attrs, ab.attrs);

        // A wide multi-byte emoji after them: explicit maps over the join.
        let emoji = make_cluster("\u{1F600}", 4, 2);
        let mixed = CellCluster::concat(&[&ab, &cd, &emoji]).unwrap();
        assert_eq!(mixed.width, 6);
        let cells: Vec<usize> = (0..8).map(|byte| mixed.byte_to_cell_idx(byte)).collect();
        assert_eq!(cells, vec![0, 1, 2, 3, 4, 4, 4, 4]);
        let widths: Vec<u8> = (0..8).map(|byte| mixed.byte_to_cell_width(byte)).collect();
        assert_eq!(widths, vec![1, 1, 1, 1, 2, 2, 2, 2]);

        // Gaps, presentation and direction changes, and nothing, are refused.
        assert!(CellCluster::concat(&[&ab, &make_cluster("e", 9, 1)]).is_none());
        let mut emoji_presentation = make_cluster("e", 2, 1);
        emoji_presentation.presentation = Presentation::Emoji;
        assert!(CellCluster::concat(&[&ab, &emoji_presentation]).is_none());
        let mut rtl = make_cluster("e", 2, 1);
        rtl.direction = Direction::RightToLeft;
        assert!(CellCluster::concat(&[&ab, &rtl]).is_none());
        assert!(CellCluster::concat(&[]).is_none());
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
    fn new_clamps_extreme_cell_width() {
        let c = make_cluster("A", 0, usize::MAX);
        assert_eq!(c.width, 2);
        assert_eq!(c.byte_to_cell_width(0), 2);
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
    fn byte_to_cell_idx_empty_map_saturates_overflow() {
        let c = make_cluster("a", usize::MAX, 1);
        assert_eq!(c.byte_to_cell_idx(1), usize::MAX);
    }

    #[test]
    fn byte_to_cell_idx_populated_map() {
        let c = make_cluster("é", 10, 1);
        // "é" is 2 bytes, map populated
        assert_eq!(c.byte_to_cell_idx(0), 10);
        assert_eq!(c.byte_to_cell_idx(1), 10);
    }

    #[test]
    fn byte_to_cell_idx_populated_map_clamps_past_end() {
        let c = make_cluster("é", 10, 1);
        assert_eq!(c.byte_to_cell_idx(usize::MAX), 10);
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

    #[test]
    fn byte_to_cell_width_populated_map_clamps_past_end() {
        let c = make_cluster("X", 0, 2);
        assert_eq!(c.byte_to_cell_width(usize::MAX), 2);
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
        for mapped in &c.byte_to_cell_idx[1..] {
            assert_eq!(*mapped, 1);
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
    fn add_clamps_extreme_width() {
        let mut c = make_cluster("a", 0, 1);
        c.add("W", 1, usize::MAX);
        assert_eq!(c.width, 3);
        assert_eq!(c.byte_to_cell_width[1], 2);
    }

    #[test]
    fn add_saturates_total_width() {
        let mut c = make_cluster("a", 0, 1);
        c.width = usize::MAX - 1;
        c.add("b", 1, 2);
        assert_eq!(c.width, usize::MAX);
    }

    #[test]
    fn add_multibyte_extrapolated_indices_saturate() {
        let mut c = make_cluster("a", usize::MAX, 1);
        c.add("é", usize::MAX, 1);
        assert_eq!(c.byte_to_cell_idx[0], usize::MAX);
        assert_eq!(c.byte_to_cell_idx[1], usize::MAX);
        assert_eq!(c.byte_to_cell_idx[2], usize::MAX);
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
