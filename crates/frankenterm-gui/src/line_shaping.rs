//! How a line's clusters are shaped, shared by the WebGpu and Metal
//! renderers so both draw the same glyphs (ft-yccm0.4.7.3):
//! - Paragraph context (ft-6scm7). When a line has more than one cluster, a
//!   cluster is shaped with the line's whole text as HarfBuzz paragraph
//!   context, so contextual lookups see its neighbors. An ASCII hyperlink
//!   cluster is the exception and is shaped on its own text.
//! - Shaping runs (ft-yccm0.4.3.4). Clusters split only by paint attributes
//!   (colors, underline, hyperlinks, ...) are shaped once, as one run, and
//!   each cluster then gets its own glyphs back ([`shape_runs`]). A cluster
//!   whose run does not split that way is shaped alone, with
//!   [`cluster_shaping_context`].

use config::TextStyle;
use std::ops::Range;
use std::rc::Rc;
use termwiz::cellcluster::CellCluster;
use wezterm_bidi::Direction;

/// A line's text, and the byte range each of its clusters takes in it.
#[derive(Debug, PartialEq, Eq)]
pub struct ClusterParagraphContext {
    pub text: String,
    pub ranges: Vec<Range<usize>>,
}

pub fn cluster_paragraph_context(cell_clusters: &[CellCluster]) -> ClusterParagraphContext {
    let mut text = String::new();
    let mut ranges = Vec::with_capacity(cell_clusters.len());

    for cluster in cell_clusters {
        let start = text.len();
        text.push_str(&cluster.text);
        ranges.push(start..text.len());
    }

    ClusterParagraphContext { text, ranges }
}

pub fn should_shape_cluster_with_paragraph_context(cluster: &CellCluster) -> bool {
    // Harfbuzz paragraph ranges are the established default for terminal
    // text. OSC 8 hyperlink runs are the narrow ASCII case that diverges from
    // upstream when shaped with adjacent non-hyperlink context.
    !cluster.text.is_ascii() || cluster.attrs.hyperlink().is_none()
}

pub fn any_cluster_needs_paragraph_context(cell_clusters: &[CellCluster]) -> bool {
    cell_clusters.len() > 1
        && cell_clusters
            .iter()
            .any(should_shape_cluster_with_paragraph_context)
}

/// The paragraph context a line's clusters are shaped with, if any.
pub fn line_paragraph_context(cell_clusters: &[CellCluster]) -> Option<ClusterParagraphContext> {
    any_cluster_needs_paragraph_context(cell_clusters)
        .then(|| cluster_paragraph_context(cell_clusters))
}

/// The paragraph text and range cluster `idx` of the line is shaped with
/// when it is shaped alone.
pub fn cluster_shaping_context<'a>(
    paragraph_context: Option<&'a ClusterParagraphContext>,
    cluster: &CellCluster,
    idx: usize,
) -> Option<(&'a str, Range<usize>)> {
    paragraph_context.and_then(|context| {
        if should_shape_cluster_with_paragraph_context(cluster) {
            Some((context.text.as_str(), context.ranges[idx].clone()))
        } else {
            None
        }
    })
}

/// Whether `next` joins `prev`'s shaping run (ft-yccm0.4.3.4). Clusters
/// split only by paint attributes (colors, underline, hyperlinks, ...) shape
/// together: same resolved font style, presentation and left-to-right
/// direction, contiguous, and the same paragraph-context treatment. A space
/// on either side of the boundary keeps `make_cluster`'s whitespace break,
/// which keeps shape-cache keys short.
pub fn joins_shaping_run(
    prev: (&CellCluster, &TextStyle),
    next: (&CellCluster, &TextStyle),
    paragraph_context: bool,
) -> bool {
    let ((prev, prev_style), (next, next_style)) = (prev, next);
    prev.attrs != next.attrs
        && std::ptr::eq(prev_style, next_style)
        && prev.presentation == next.presentation
        && prev.direction == Direction::LeftToRight
        && next.direction == Direction::LeftToRight
        && next.first_cell_idx == prev.first_cell_idx + prev.width
        && !prev.text.ends_with(' ')
        && !next.text.starts_with(' ')
        && (!paragraph_context
            || should_shape_cluster_with_paragraph_context(prev)
                == should_shape_cluster_with_paragraph_context(next))
}

/// The line's shaping runs as ranges of cluster indices.
pub fn shaping_runs(
    clusters: &[CellCluster],
    styles: &[&TextStyle],
    paragraph_context: bool,
) -> Vec<Range<usize>> {
    let mut runs = Vec::new();
    let mut start = 0;
    for idx in 1..=clusters.len() {
        let joins = idx < clusters.len()
            && joins_shaping_run(
                (&clusters[idx - 1], styles[idx - 1]),
                (&clusters[idx], styles[idx]),
                paragraph_context,
            );
        if !joins {
            runs.push(start..idx);
            start = idx;
        }
    }
    runs
}

/// Splits a shaped run's glyphs back among the clusters it joined: the glyph
/// index range each cluster gets, from every glyph's cell count in order and
/// each cluster's `(first cell, width)`. Zero-cell glyphs (marks) stay with
/// the glyph before them. `None` when a glyph spans two clusters (a ligature
/// across a paint boundary), the cells do not add up, or a cluster would get
/// no glyph; the caller then shapes those clusters one by one, as before.
pub fn split_glyph_cells(cells: &[u8], parts: &[(usize, usize)]) -> Option<Vec<Range<usize>>> {
    let (&(first_cell, _), _) = parts.split_first()?;
    let end = |part: usize| parts[part].0 + parts[part].1;
    let mut ranges = Vec::with_capacity(parts.len());
    let mut part = 0;
    let mut part_start = 0;
    let mut cell = first_cell;
    for (glyph, &width) in cells.iter().enumerate() {
        let width = usize::from(width);
        if width == 0 {
            continue;
        }
        while part < parts.len() && cell >= end(part) {
            ranges.push(part_start..glyph);
            part_start = glyph;
            part += 1;
        }
        if part == parts.len() || cell < parts[part].0 || cell + width > end(part) {
            return None;
        }
        cell += width;
    }
    if part + 1 != parts.len() || cell != end(part) {
        return None;
    }
    ranges.push(part_start..cells.len());
    ranges
        .iter()
        .all(|range| !range.is_empty())
        .then_some(ranges)
}

/// The glyphs [`shape_runs`] got for a line's clusters.
#[derive(Debug)]
pub struct RunGlyphs<G> {
    /// Per cluster: its glyphs from its run, or `None` when the caller shapes
    /// it alone (with [`cluster_shaping_context`]) because its run has one
    /// cluster or did not split at its cluster boundaries.
    pub glyphs: Vec<Option<Rc<Vec<G>>>>,
    /// Shaper calls made: one per run of two or more clusters.
    pub runs: u64,
    /// Runs shaped but not split, whose clusters are shaped again alone.
    pub unsplit_runs: u64,
}

/// Shapes the line's runs of two or more clusters (ft-yccm0.4.3.4) and
/// splits each run's glyphs back among its clusters ([`split_glyph_cells`]).
/// A run is the clusters joined with `CellCluster::concat`, shaped with the
/// first cluster's style, and with the paragraph text over the run's byte
/// range when its clusters take paragraph context. `num_cells` gives a
/// glyph's shaper cell count; `shape` shapes a cluster with that style and
/// context.
pub fn shape_runs<G: Clone, E>(
    clusters: &[CellCluster],
    styles: &[&TextStyle],
    paragraph_context: Option<&ClusterParagraphContext>,
    num_cells: impl Fn(&G) -> u8,
    mut shape: impl FnMut(
        &TextStyle,
        &CellCluster,
        Option<(&str, Range<usize>)>,
    ) -> Result<Rc<Vec<G>>, E>,
) -> Result<RunGlyphs<G>, E> {
    let mut shaped = RunGlyphs {
        glyphs: vec![None; clusters.len()],
        runs: 0,
        unsplit_runs: 0,
    };
    for run in shaping_runs(clusters, styles, paragraph_context.is_some()) {
        if run.len() < 2 {
            continue;
        }
        let parts: Vec<&CellCluster> = clusters[run.clone()].iter().collect();
        let Some(joined) = CellCluster::concat(&parts) else {
            continue;
        };
        let run_context = paragraph_context
            .filter(|_| should_shape_cluster_with_paragraph_context(parts[0]))
            .map(|context| {
                (
                    context.text.as_str(),
                    context.ranges[run.start].start..context.ranges[run.end - 1].end,
                )
            });
        let shaped_run = shape(styles[run.start], &joined, run_context)?;
        shaped.runs += 1;
        let cells: Vec<u8> = shaped_run.iter().map(&num_cells).collect();
        let extents: Vec<(usize, usize)> = parts
            .iter()
            .map(|part| (part.first_cell_idx, part.width))
            .collect();
        match split_glyph_cells(&cells, &extents) {
            Some(ranges) => {
                for (offset, range) in ranges.into_iter().enumerate() {
                    shaped.glyphs[run.start + offset] = Some(Rc::new(shaped_run[range].to_vec()));
                }
            }
            None => shaped.unsplit_runs += 1,
        }
    }
    Ok(shaped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use termwiz::cell::{Cell, CellAttributes, Intensity};
    use termwiz::color::ColorAttribute;
    use termwiz::hyperlink::Hyperlink;
    use termwiz::surface::{Line, SEQ_ZERO};

    #[test]
    fn cluster_paragraph_context_tracks_rtl_and_indic_ranges() {
        let attrs = CellAttributes::default();
        let line = Line::from_text("abc שלום नमस्ते", &attrs, SEQ_ZERO, None);
        let clusters = line.cluster(None);
        let context = cluster_paragraph_context(&clusters);

        assert_eq!(context.ranges.len(), clusters.len());
        assert!(context.text.contains("שלום"));
        assert!(context.text.contains("नमस्ते"));

        let mut expected_start = 0;
        for (cluster, range) in clusters.iter().zip(&context.ranges) {
            assert_eq!(range.start, expected_start);
            assert_eq!(&context.text[range.clone()], cluster.text);
            expected_start = range.end;
        }
        assert_eq!(expected_start, context.text.len());
    }

    #[test]
    fn paragraph_context_skips_only_ascii_hyperlink_clusters() {
        let attrs = CellAttributes::default();

        let ascii = Line::from_text("linktext", &attrs, SEQ_ZERO, None);
        let ascii_clusters = ascii.cluster(None);
        assert_eq!(ascii_clusters.len(), 1);
        assert!(!any_cluster_needs_paragraph_context(&ascii_clusters));
        assert!(
            ascii_clusters
                .iter()
                .all(should_shape_cluster_with_paragraph_context)
        );

        // With bidi disabled, whitespace boundaries deliberately split runs.
        // They need paragraph context just like attribute-split ASCII runs.
        let spaced_ascii = Line::from_text("link text", &attrs, SEQ_ZERO, None);
        let spaced_clusters = spaced_ascii.cluster(None);
        assert!(spaced_clusters.len() > 1);
        assert!(any_cluster_needs_paragraph_context(&spaced_clusters));

        let mut bold_attrs = CellAttributes::default();
        bold_attrs.set_intensity(Intensity::Bold);
        let ascii_split = Line::from_cells(
            vec![
                Cell::new('l', attrs.clone()),
                Cell::new('i', attrs.clone()),
                Cell::new('n', bold_attrs.clone()),
                Cell::new('k', bold_attrs),
            ],
            SEQ_ZERO,
        );
        let ascii_split_clusters = ascii_split.cluster(None);
        assert!(ascii_split_clusters.len() > 1);
        assert!(any_cluster_needs_paragraph_context(&ascii_split_clusters));

        let mut hyperlink_attrs = CellAttributes::default();
        hyperlink_attrs.set_hyperlink(Some(Arc::new(Hyperlink::new("https://example.com"))));
        let hyperlink = Line::from_text("link text", &hyperlink_attrs, SEQ_ZERO, None);
        let hyperlink_clusters = hyperlink.cluster(None);
        assert!(!any_cluster_needs_paragraph_context(&hyperlink_clusters));
        assert!(
            hyperlink_clusters
                .iter()
                .all(|cluster| !should_shape_cluster_with_paragraph_context(cluster))
        );

        let complex = Line::from_text("abc שלום नमस्ते", &attrs, SEQ_ZERO, None);
        let complex_clusters = complex.cluster(None);
        assert!(any_cluster_needs_paragraph_context(&complex_clusters));
        assert!(complex_clusters.iter().any(|cluster| {
            !cluster.text.is_ascii() && should_shape_cluster_with_paragraph_context(cluster)
        }));
        assert!(
            complex_clusters
                .iter()
                .all(should_shape_cluster_with_paragraph_context)
        );
    }

    fn colored(index: u8) -> CellAttributes {
        let mut attrs = CellAttributes::default();
        attrs.set_foreground(ColorAttribute::PaletteIndex(index));
        attrs
    }

    fn runs_of(cells: Vec<Cell>, styles: &[&TextStyle]) -> Vec<Range<usize>> {
        let clusters = Line::from_cells(cells, SEQ_ZERO).cluster(None);
        assert_eq!(clusters.len(), styles.len(), "{clusters:?}");
        shaping_runs(
            &clusters,
            styles,
            any_cluster_needs_paragraph_context(&clusters),
        )
    }

    /// ft-yccm0.4.3.4: color changes no longer split shaping runs; font
    /// style, presentation and whitespace still do.
    #[test]
    fn shaping_runs_join_paint_only_attribute_changes() {
        let style = TextStyle::default();
        let bold = TextStyle::default();
        let same = [&style, &style, &style];

        // Three colors, one font: one run.
        let rainbow = vec![
            Cell::new('a', colored(1)),
            Cell::new('b', colored(1)),
            Cell::new('c', colored(2)),
            Cell::new('d', colored(3)),
        ];
        assert_eq!(runs_of(rainbow.clone(), &same), vec![0..3]);
        // A different resolved style (font_rules, bold) splits the run.
        assert_eq!(
            runs_of(rainbow.clone(), &[&style, &bold, &bold]),
            vec![0..1, 1..3]
        );

        // make_cluster's whitespace break stays, even across a color change.
        let spaced = vec![
            Cell::new('a', colored(1)),
            Cell::new(' ', colored(1)),
            Cell::new('b', colored(2)),
        ];
        assert_eq!(runs_of(spaced, &[&style, &style]), vec![0..1, 1..2]);

        // Text and emoji presentation never share a run.
        let emoji = vec![
            Cell::new('a', colored(1)),
            Cell::new_grapheme("\u{1F600}", colored(2), None),
        ];
        assert_eq!(runs_of(emoji, &[&style, &style]), vec![0..1, 1..2]);

        // An ASCII hyperlink keeps its own shaping (no paragraph context).
        let mut linked = colored(2);
        linked.set_hyperlink(Some(Arc::new(Hyperlink::new("https://example.com"))));
        let hyperlink = vec![Cell::new('a', colored(1)), Cell::new('b', linked)];
        assert_eq!(runs_of(hyperlink, &[&style, &style]), vec![0..1, 1..2]);

        assert!(shaping_runs(&[], &[], false).is_empty());
    }

    #[test]
    fn split_glyph_cells_gives_each_cluster_its_own_glyphs() {
        let parts = [(0, 2), (2, 2), (4, 1)];
        assert_eq!(
            split_glyph_cells(&[1, 1, 1, 1, 1], &parts),
            Some(vec![0..2, 2..4, 4..5])
        );
        // A zero-cell mark stays with the glyph before it.
        assert_eq!(
            split_glyph_cells(&[1, 0, 1, 1, 1, 1], &parts),
            Some(vec![0..3, 3..5, 5..6])
        );
        // Wide glyphs.
        assert_eq!(
            split_glyph_cells(&[2, 2], &[(10, 2), (12, 2)]),
            Some(vec![0..1, 1..2])
        );
        // A ligature across the paint boundary cannot be split.
        assert_eq!(split_glyph_cells(&[3, 1], &[(0, 2), (2, 2)]), None);
        // Cells that do not add up, or nothing to split.
        assert_eq!(split_glyph_cells(&[1, 1, 1], &[(0, 2), (2, 2)]), None);
        assert_eq!(split_glyph_cells(&[1, 1], &[]), None);
    }

    /// A synthetic shaper call: what was shaped and with which context.
    #[derive(Debug, Clone, PartialEq)]
    struct Call {
        text: String,
        context: Option<(String, Range<usize>)>,
    }

    /// One glyph per character, one cell each, except that "->" is one
    /// glyph two cells wide (a ligature).
    fn synthetic_shape(cluster: &CellCluster) -> Rc<Vec<(String, u8)>> {
        let mut glyphs = Vec::new();
        let mut chars = cluster.text.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '-' && chars.peek() == Some(&'>') {
                chars.next();
                glyphs.push(("->".to_string(), 2));
            } else {
                glyphs.push((c.to_string(), 1));
            }
        }
        Rc::new(glyphs)
    }

    fn shape_line_runs(cells: Vec<Cell>) -> (RunGlyphs<(String, u8)>, Vec<Call>) {
        let clusters = Line::from_cells(cells, SEQ_ZERO).cluster(None);
        let style = TextStyle::default();
        let styles = vec![&style; clusters.len()];
        let context = line_paragraph_context(&clusters);
        let mut calls = Vec::new();
        let shaped = shape_runs(
            &clusters,
            &styles,
            context.as_ref(),
            |glyph: &(String, u8)| glyph.1,
            |_, cluster, context| {
                calls.push(Call {
                    text: cluster.text.clone(),
                    context: context.map(|(text, range)| (text.to_string(), range)),
                });
                Ok::<_, ()>(synthetic_shape(cluster))
            },
        )
        .unwrap();
        (shaped, calls)
    }

    fn texts(glyphs: &Option<Rc<Vec<(String, u8)>>>) -> Option<Vec<&str>> {
        glyphs
            .as_ref()
            .map(|glyphs| glyphs.iter().map(|(text, _)| text.as_str()).collect())
    }

    /// A run is shaped once, over its byte range of the line's text, and
    /// each cluster gets the glyphs in its cells; a single-cluster run is
    /// left to the caller.
    #[test]
    fn shape_runs_split_a_run_among_its_clusters() {
        // Clusters "a", "bc " and "d": the trailing space keeps "d" out of
        // the run.
        let cells = vec![
            Cell::new('a', colored(1)),
            Cell::new('b', colored(2)),
            Cell::new('c', colored(2)),
            Cell::new(' ', colored(2)),
            Cell::new('d', colored(3)),
        ];
        let (shaped, calls) = shape_line_runs(cells);
        assert_eq!(
            calls,
            vec![Call {
                text: "abc ".to_string(),
                context: Some(("abc d".to_string(), 0..4)),
            }]
        );
        assert_eq!(shaped.glyphs.len(), 3);
        assert_eq!(texts(&shaped.glyphs[0]), Some(vec!["a"]));
        assert_eq!(texts(&shaped.glyphs[1]), Some(vec!["b", "c", " "]));
        assert_eq!(shaped.glyphs[2], None);
        assert_eq!((shaped.runs, shaped.unsplit_runs), (1, 0));
    }

    /// A ligature across a paint boundary does not split, so the run's
    /// clusters are left to be shaped alone (where no ligature forms).
    #[test]
    fn shape_runs_leave_a_ligature_across_clusters_unsplit() {
        let cells = vec![
            Cell::new('a', colored(1)),
            Cell::new('-', colored(1)),
            Cell::new('>', colored(2)),
            Cell::new('b', colored(2)),
        ];
        let (shaped, calls) = shape_line_runs(cells);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].text, "a->b");
        assert_eq!(shaped.glyphs, vec![None, None]);
        assert_eq!((shaped.runs, shaped.unsplit_runs), (1, 1));
    }

    /// An ASCII hyperlink cluster has no paragraph context alone, and a run
    /// of hyperlink clusters none either.
    #[test]
    fn hyperlink_clusters_are_shaped_without_context() {
        let mut first = colored(1);
        first.set_hyperlink(Some(Arc::new(Hyperlink::new("https://example.com"))));
        let mut second = colored(2);
        second.set_hyperlink(first.hyperlink().cloned());
        let cells = vec![
            Cell::new('x', colored(3)),
            Cell::new(' ', colored(3)),
            Cell::new('a', first),
            Cell::new('b', second),
        ];
        let clusters = Line::from_cells(cells.clone(), SEQ_ZERO).cluster(None);
        let context = line_paragraph_context(&clusters);
        assert!(context.is_some());
        assert_eq!(
            cluster_shaping_context(context.as_ref(), &clusters[0], 0),
            Some(("x ab", 0..2))
        );
        assert_eq!(
            cluster_shaping_context(context.as_ref(), &clusters[1], 1),
            None
        );
        let (shaped, calls) = shape_line_runs(cells);
        assert_eq!(
            calls,
            vec![Call {
                text: "ab".to_string(),
                context: None,
            }]
        );
        assert_eq!(texts(&shaped.glyphs[1]), Some(vec!["a"]));
        assert_eq!(texts(&shaped.glyphs[2]), Some(vec!["b"]));
    }
}
