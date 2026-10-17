//! Shaping bypasses (ft-yccm0.4.3.3).
//!
//! A run whose clusters are single codepoints, each with at most a text or
//! emoji presentation selector, is laid out from what each cluster got from
//! HarfBuzz before, without calling HarfBuzz, when HarfBuzz could not have
//! laid it out any other way: no lookup the face applies can match a
//! sequence of the run's glyphs, so each cluster gets exactly the glyphs,
//! advances and offsets it gets in any other run.
//!
//! [`LayoutGate`] finds the glyphs of a face that qualify, once per face,
//! from its GSUB and GPOS tables and the features HarfBuzz applies. A
//! ligature, contextual or kerning lookup that can match among those glyphs
//! takes the glyphs it needs out of the set, so runs that could form a
//! ligature or a contextual alternate still go to HarfBuzz.
//!
//! [`ClusterCache`] holds each face's clusters: a table for printable ASCII
//! and a bounded map for every other cluster. A run that misses some
//! clusters is shaped by HarfBuzz as before, and its output fills them.

use crate::hbwrap as harfbuzz;
use harfbuzz::hb_script_t::{self, *};
use harfbuzz::hb_unicode_general_category_t::*;
use std::collections::HashMap;

/// One glyph as HarfBuzz lays it out: `cluster` is a byte offset into the
/// shaped text, positions are 26.6 fixed point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RawGlyph {
    pub glyph: u32,
    pub cluster: u32,
    pub x_advance: i32,
    pub y_advance: i32,
    pub x_offset: i32,
    pub y_offset: i32,
}

/// A set of glyph ids.
#[derive(Clone, Debug, Default)]
pub(crate) struct GlyphSet {
    words: Vec<u64>,
}

impl PartialEq for GlyphSet {
    fn eq(&self, other: &Self) -> bool {
        self.iter().eq(other.iter())
    }
}

impl Eq for GlyphSet {}

impl GlyphSet {
    pub(crate) fn contains(&self, glyph: u32) -> bool {
        self.words
            .get((glyph / 64) as usize)
            .is_some_and(|word| word & (1 << (glyph % 64)) != 0)
    }

    pub(crate) fn insert(&mut self, glyph: u32) -> bool {
        let index = (glyph / 64) as usize;
        if index >= self.words.len() {
            self.words.resize(index + 1, 0);
        }
        let bit = 1 << (glyph % 64);
        let added = self.words[index] & bit == 0;
        self.words[index] |= bit;
        added
    }

    fn remove(&mut self, glyph: u32) {
        if let Some(word) = self.words.get_mut((glyph / 64) as usize) {
            *word &= !(1 << (glyph % 64));
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.words
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.words.iter().all(|&word| word == 0)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.words.iter().enumerate().flat_map(|(index, &word)| {
            let mut word = word;
            std::iter::from_fn(move || {
                (word != 0).then(|| {
                    let bit = word.trailing_zeros();
                    word &= word - 1;
                    index as u32 * 64 + bit
                })
            })
        })
    }
}

const fn tag(name: &[u8; 4]) -> u32 {
    harfbuzz::hb_tag(name[0], name[1], name[2], name[3])
}

const RAND: u32 = tag(b"rand");

/// The features HarfBuzz 11 turns on for horizontal left-to-right text with
/// its default shaper (hb-ot-shape.cc `hb_ot_shape_collect_features`).
/// `frac`, `numr` and `dnom` apply only around U+2044, which no bypassed run
/// contains, and `rand` picks alternates at random.
const DEFAULT_FEATURES: [&[u8; 4]; 22] = [
    b"rvrn", b"ltra", b"ltrm", b"trak", b"Harf", b"HARF", b"Buzz", b"BUZZ", b"abvm", b"blwm",
    b"ccmp", b"locl", b"mark", b"mkmk", b"rlig", b"calt", b"clig", b"curs", b"dist", b"kern",
    b"liga", b"rclt",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FeatureUse {
    Off,
    On,
    /// `rand`: each glyph gets an alternate chosen at random.
    Random,
}

/// The features a face is shaped with.
#[derive(Clone, Debug)]
pub(crate) struct Features {
    tags: HashMap<u32, FeatureUse>,
}

impl Features {
    /// HarfBuzz's default features with `user`'s applied over them in
    /// order, or `None` if a user feature covers only part of the text,
    /// which makes a cluster's layout depend on where it is.
    pub(crate) fn applied(user: &[harfbuzz::hb_feature_t]) -> Option<Self> {
        let mut tags: HashMap<u32, FeatureUse> = DEFAULT_FEATURES
            .iter()
            .map(|name| (tag(name), FeatureUse::On))
            .collect();
        tags.insert(RAND, FeatureUse::Random);
        for feature in user {
            if feature.start != 0 || feature.end != u32::MAX {
                return None;
            }
            let state = match (feature.value, feature.tag) {
                (0, _) => FeatureUse::Off,
                (_, RAND) => FeatureUse::Random,
                _ => FeatureUse::On,
            };
            tags.insert(feature.tag, state);
        }
        Some(Self { tags })
    }

    fn state(&self, tag: u32) -> FeatureUse {
        self.tags.get(&tag).copied().unwrap_or(FeatureUse::Off)
    }
}

/// Bounds-checked big-endian reads from an OpenType table. Every read that
/// fails makes the whole analysis fail, which disengages the bypass.
#[derive(Clone, Copy)]
struct Table<'a>(&'a [u8]);

impl<'a> Table<'a> {
    fn u16(self, offset: usize) -> Option<u16> {
        let bytes = self.0.get(offset..offset.checked_add(2)?)?;
        Some(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn u32(self, offset: usize) -> Option<u32> {
        let bytes = self.0.get(offset..offset.checked_add(4)?)?;
        Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn at(self, offset: usize) -> Option<Table<'a>> {
        self.0.get(offset..).map(Table)
    }

    /// The table a 16-bit offset at `field` points to; `None` for a null
    /// offset.
    fn follow(self, field: usize) -> Option<Table<'a>> {
        match self.u16(field)? {
            0 => None,
            offset => self.at(usize::from(offset)),
        }
    }

    /// `count` 16-bit values from `offset`.
    fn u16s(self, offset: usize, count: usize) -> Option<Vec<u16>> {
        (0..count).map(|i| self.u16(offset + 2 * i)).collect()
    }
}

/// A Coverage table's glyphs in coverage-index order.
fn coverage(table: Table) -> Option<Vec<u16>> {
    match table.u16(0)? {
        1 => table.u16s(4, usize::from(table.u16(2)?)),
        2 => {
            let mut glyphs = Vec::new();
            for range in 0..usize::from(table.u16(2)?) {
                let record = 4 + 6 * range;
                let (start, end) = (table.u16(record)?, table.u16(record + 2)?);
                if end < start || usize::from(table.u16(record + 4)?) != glyphs.len() {
                    return None;
                }
                glyphs.extend(start..=end);
            }
            Some(glyphs)
        }
        _ => None,
    }
}

/// A ClassDef table: the glyphs of each nonzero class. Every other glyph is
/// in class 0.
#[derive(Debug, Default)]
struct ClassDef {
    members: HashMap<u16, Vec<u16>>,
    class_of: HashMap<u16, u16>,
}

impl ClassDef {
    fn read(table: Option<Table>) -> Option<Self> {
        let mut def = Self::default();
        // A null ClassDef puts every glyph in class 0.
        let Some(table) = table else {
            return Some(def);
        };
        let mut add = |glyph: u16, class: u16| {
            if class != 0 {
                def.members.entry(class).or_default().push(glyph);
                def.class_of.insert(glyph, class);
            }
        };
        match table.u16(0)? {
            1 => {
                let start = table.u16(2)?;
                let classes = table.u16s(6, usize::from(table.u16(4)?))?;
                for (glyph, class) in (start..=u16::MAX).zip(classes) {
                    add(glyph, class);
                }
            }
            2 => {
                for range in 0..usize::from(table.u16(2)?) {
                    let record = 4 + 6 * range;
                    let (start, end) = (table.u16(record)?, table.u16(record + 2)?);
                    let class = table.u16(record + 4)?;
                    for glyph in start..=end {
                        add(glyph, class);
                    }
                }
            }
            _ => return None,
        }
        Some(def)
    }

    fn class(&self, glyph: u16) -> u16 {
        self.class_of.get(&glyph).copied().unwrap_or(0)
    }
}

/// The glyphs one position of a rule can match.
#[derive(Debug)]
enum Position {
    Glyphs(Vec<u16>),
    /// The glyphs of `class` in class definition `def`.
    Class {
        def: usize,
        class: u16,
    },
    Any,
}

/// The lookups a face applies, reduced to what decides whether clusters
/// compose: substitutions of one glyph by others, which act on a cluster
/// alone, and rules, sequences of positions that must all match for a
/// lookup to act across neighbouring glyphs.
#[derive(Debug, Default)]
struct Layout {
    class_defs: Vec<ClassDef>,
    rules: Vec<Vec<Position>>,
    /// Single-glyph substitutions as (from, to).
    edges: Vec<(u16, u16)>,
    /// Glyph ids read so far, bounding the work a hostile table can cause.
    read: usize,
}

/// More glyph ids than any real font's lookups hold.
const READ_BUDGET: usize = 8_000_000;

const GSUB_EXTENSION: u16 = 7;
const GPOS_EXTENSION: u16 = 9;

impl Layout {
    fn budget(&mut self, glyphs: usize) -> Option<()> {
        self.read += glyphs;
        (self.read <= READ_BUDGET).then_some(())
    }

    fn rule(&mut self, positions: Vec<Position>) -> Option<()> {
        let glyphs = positions
            .iter()
            .map(|position| match position {
                Position::Glyphs(glyphs) => glyphs.len(),
                _ => 1,
            })
            .sum();
        self.budget(glyphs)?;
        self.rules.push(positions);
        Some(())
    }

    fn edges(&mut self, from: u16, to: impl IntoIterator<Item = u16>) -> Option<()> {
        let before = self.edges.len();
        self.edges.extend(to.into_iter().map(|to| (from, to)));
        self.budget(self.edges.len() - before)
    }

    fn class_def(&mut self, table: Option<Table>) -> Option<usize> {
        self.class_defs.push(ClassDef::read(table)?);
        Some(self.class_defs.len() - 1)
    }

    /// Reads the lookups of a GSUB or GPOS table that `features` reach.
    fn read_table(&mut self, data: &[u8], gsub: bool, features: &Features) -> Option<()> {
        if data.is_empty() {
            return Some(());
        }
        let table = Table(data);
        let lookups = table.follow(8)?;
        let lookup_count = lookups.u16(0)?;
        let mut reached: HashMap<u16, bool> = HashMap::new();
        if table.u16(2)? >= 1 && table.u32(10)? != 0 {
            // Feature variations can swap in other lookups for a feature:
            // take every lookup, all of them possibly random.
            reached.extend((0..lookup_count).map(|index| (index, true)));
        } else {
            let feature_list = table.follow(6)?;
            let feature_count = feature_list.u16(0)?;
            let mut wanted: Vec<u16> = (0..feature_count)
                .filter(|&index| {
                    feature_list
                        .u32(2 + 6 * usize::from(index))
                        .is_some_and(|tag| features.state(tag) != FeatureUse::Off)
                })
                .collect();
            // Every language system's required feature applies whatever the
            // features are.
            let scripts = table.follow(4)?;
            for script in 0..usize::from(scripts.u16(0)?) {
                let script = scripts.follow(2 + 6 * script + 4)?;
                let mut systems = vec![];
                if script.u16(0)? != 0 {
                    systems.push(script.follow(0)?);
                }
                for system in 0..usize::from(script.u16(2)?) {
                    systems.push(script.follow(4 + 6 * system + 4)?);
                }
                for system in systems {
                    let required = system.u16(2)?;
                    if required != 0xFFFF {
                        wanted.push(required);
                    }
                }
            }
            for index in wanted {
                let record = 2 + 6 * usize::from(index);
                let tag = feature_list.u32(record)?;
                let random = features.state(tag) == FeatureUse::Random;
                let feature = feature_list.follow(record + 4)?;
                for lookup in feature.u16s(4, usize::from(feature.u16(2)?))? {
                    if lookup >= lookup_count {
                        return None;
                    }
                    *reached.entry(lookup).or_default() |= random;
                }
            }
        }
        for (index, random) in reached {
            let lookup = lookups.follow(2 + 2 * usize::from(index))?;
            let kind = lookup.u16(0)?;
            for subtable in 0..usize::from(lookup.u16(4)?) {
                let subtable = lookup.follow(6 + 2 * subtable)?;
                self.read_subtable(subtable, kind, gsub, random)?;
            }
        }
        Some(())
    }

    fn read_subtable(&mut self, table: Table, kind: u16, gsub: bool, random: bool) -> Option<()> {
        match (gsub, kind) {
            (true, GSUB_EXTENSION) | (false, GPOS_EXTENSION) => {
                let inner = table.u16(2)?;
                if inner == kind {
                    return None;
                }
                let target = table.at(usize::try_from(table.u32(4)?).ok()?)?;
                self.read_subtable(target, inner, gsub, random)
            }
            (true, 1..=3) => {
                let covered = coverage(table.follow(2)?)?;
                if random {
                    // A random alternate depends on where the glyph is.
                    self.rule(vec![Position::Glyphs(covered.clone())])?;
                }
                match (kind, table.u16(0)?) {
                    (1, 1) => {
                        let delta = table.u16(4)?;
                        for glyph in covered {
                            self.edges(glyph, [glyph.wrapping_add(delta)])?;
                        }
                    }
                    (1, 2) => {
                        let substitutes = table.u16s(6, usize::from(table.u16(4)?))?;
                        for (glyph, substitute) in covered.into_iter().zip(substitutes) {
                            self.edges(glyph, [substitute])?;
                        }
                    }
                    // Multiple substitution sequences and alternate sets
                    // have the same shape.
                    (2 | 3, 1) => {
                        let set_count = usize::from(table.u16(4)?);
                        for (index, glyph) in covered.into_iter().enumerate().take(set_count) {
                            let set = table.follow(6 + 2 * index)?;
                            let outputs = set.u16s(2, usize::from(set.u16(0)?))?;
                            self.edges(glyph, outputs)?;
                        }
                    }
                    _ => return None,
                }
                Some(())
            }
            (true, 4) => {
                if table.u16(0)? != 1 {
                    return None;
                }
                let covered = coverage(table.follow(2)?)?;
                let set_count = usize::from(table.u16(4)?);
                for (index, first) in covered.into_iter().enumerate().take(set_count) {
                    let set = table.follow(6 + 2 * index)?;
                    for ligature in 0..usize::from(set.u16(0)?) {
                        let ligature = set.follow(2 + 2 * ligature)?;
                        let glyph = ligature.u16(0)?;
                        let components = usize::from(ligature.u16(2)?);
                        if components <= 1 {
                            self.edges(first, [glyph])?;
                            continue;
                        }
                        let mut positions = vec![Position::Glyphs(vec![first])];
                        for component in ligature.u16s(4, components - 1)? {
                            positions.push(Position::Glyphs(vec![component]));
                        }
                        self.rule(positions)?;
                    }
                }
                Some(())
            }
            (true, 5) | (false, 7) => self.read_context(table),
            (true, 6) | (false, 8) => self.read_chained_context(table),
            (true, 8) => {
                if table.u16(0)? != 1 {
                    return None;
                }
                let mut positions = vec![Position::Glyphs(coverage(table.follow(2)?)?)];
                let backtrack = usize::from(table.u16(4)?);
                let lookahead_at = 6 + 2 * backtrack;
                let lookahead = usize::from(table.u16(lookahead_at)?);
                for field in (0..backtrack)
                    .map(|i| 6 + 2 * i)
                    .chain((0..lookahead).map(|i| lookahead_at + 2 + 2 * i))
                {
                    positions.push(Position::Glyphs(coverage(table.follow(field)?)?));
                }
                self.rule(positions)
            }
            // A single adjustment moves one glyph wherever it is.
            (false, 1) => Some(()),
            (false, 2) => {
                let covered = coverage(table.follow(2)?)?;
                match table.u16(0)? {
                    1 => {
                        let value_size = |format: u16| 2 * (format & 0xFF).count_ones() as usize;
                        let record_size = 2 + value_size(table.u16(4)?) + value_size(table.u16(6)?);
                        let set_count = usize::from(table.u16(8)?);
                        for (index, first) in covered.into_iter().enumerate().take(set_count) {
                            let set = table.follow(10 + 2 * index)?;
                            let seconds = (0..usize::from(set.u16(0)?))
                                .map(|pair| set.u16(2 + record_size * pair))
                                .collect::<Option<Vec<u16>>>()?;
                            self.rule(vec![
                                Position::Glyphs(vec![first]),
                                Position::Glyphs(seconds),
                            ])?;
                        }
                        Some(())
                    }
                    2 => self.rule(vec![Position::Glyphs(covered), Position::Any]),
                    _ => None,
                }
            }
            (false, 3) => {
                let covered = coverage(table.follow(2)?)?;
                self.rule(vec![
                    Position::Glyphs(covered.clone()),
                    Position::Glyphs(covered),
                ])
            }
            // Mark attachment acts at a mark glyph (the first coverage) and
            // moves it onto a glyph before it, possibly another cluster's.
            (false, 4..=6) => {
                let marks = coverage(table.follow(2)?)?;
                self.rule(vec![Position::Glyphs(marks)])
            }
            _ => None,
        }
    }

    /// GSUB type 5 and GPOS type 7.
    fn read_context(&mut self, table: Table) -> Option<()> {
        match table.u16(0)? {
            1 => {
                let covered = coverage(table.follow(2)?)?;
                let set_count = usize::from(table.u16(4)?);
                for (index, first) in covered.into_iter().enumerate().take(set_count) {
                    let Some(set) = table.follow(6 + 2 * index) else {
                        continue;
                    };
                    for rule in 0..usize::from(set.u16(0)?) {
                        let rule = set.follow(2 + 2 * rule)?;
                        let count = usize::from(rule.u16(0)?);
                        let mut positions = vec![Position::Glyphs(vec![first])];
                        for glyph in rule.u16s(4, count.checked_sub(1)?)? {
                            positions.push(Position::Glyphs(vec![glyph]));
                        }
                        self.rule(positions)?;
                    }
                }
                Some(())
            }
            2 => {
                let covered = coverage(table.follow(2)?)?;
                let def = self.class_def(table.follow(4))?;
                for class in 0..table.u16(6)? {
                    let Some(set) = table.follow(8 + 2 * usize::from(class)) else {
                        continue;
                    };
                    let first: Vec<u16> = covered
                        .iter()
                        .copied()
                        .filter(|&glyph| self.class_defs[def].class(glyph) == class)
                        .collect();
                    for rule in 0..usize::from(set.u16(0)?) {
                        let rule = set.follow(2 + 2 * rule)?;
                        let count = usize::from(rule.u16(0)?);
                        let mut positions = vec![Position::Glyphs(first.clone())];
                        for class in rule.u16s(4, count.checked_sub(1)?)? {
                            positions.push(Position::Class { def, class });
                        }
                        self.rule(positions)?;
                    }
                }
                Some(())
            }
            3 => {
                let count = usize::from(table.u16(2)?);
                let positions = (0..count)
                    .map(|i| Some(Position::Glyphs(coverage(table.follow(6 + 2 * i)?)?)))
                    .collect::<Option<Vec<_>>>()?;
                self.rule(positions)
            }
            _ => None,
        }
    }

    /// GSUB type 6 and GPOS type 8.
    fn read_chained_context(&mut self, table: Table) -> Option<()> {
        // A rule's backtrack, input after the first and lookahead sequences
        // follow one another, each after its count.
        let sequences = |rule: Table| -> Option<[Vec<u16>; 3]> {
            let backtrack = usize::from(rule.u16(0)?);
            let input_at = 2 + 2 * backtrack;
            let input = usize::from(rule.u16(input_at)?).checked_sub(1)?;
            let lookahead_at = input_at + 2 + 2 * input;
            let lookahead = usize::from(rule.u16(lookahead_at)?);
            Some([
                rule.u16s(2, backtrack)?,
                rule.u16s(input_at + 2, input)?,
                rule.u16s(lookahead_at + 2, lookahead)?,
            ])
        };
        match table.u16(0)? {
            1 => {
                let covered = coverage(table.follow(2)?)?;
                let set_count = usize::from(table.u16(4)?);
                for (index, first) in covered.into_iter().enumerate().take(set_count) {
                    let Some(set) = table.follow(6 + 2 * index) else {
                        continue;
                    };
                    for rule in 0..usize::from(set.u16(0)?) {
                        let rule = set.follow(2 + 2 * rule)?;
                        let mut positions = vec![Position::Glyphs(vec![first])];
                        for glyph in sequences(rule)?.into_iter().flatten() {
                            positions.push(Position::Glyphs(vec![glyph]));
                        }
                        self.rule(positions)?;
                    }
                }
                Some(())
            }
            2 => {
                let covered = coverage(table.follow(2)?)?;
                let defs = [
                    self.class_def(table.follow(4))?,
                    self.class_def(table.follow(6))?,
                    self.class_def(table.follow(8))?,
                ];
                for class in 0..table.u16(10)? {
                    let Some(set) = table.follow(12 + 2 * usize::from(class)) else {
                        continue;
                    };
                    let first: Vec<u16> = covered
                        .iter()
                        .copied()
                        .filter(|&glyph| self.class_defs[defs[1]].class(glyph) == class)
                        .collect();
                    for rule in 0..usize::from(set.u16(0)?) {
                        let rule = set.follow(2 + 2 * rule)?;
                        let mut positions = vec![Position::Glyphs(first.clone())];
                        for (def, classes) in defs.into_iter().zip(sequences(rule)?) {
                            for class in classes {
                                positions.push(Position::Class { def, class });
                            }
                        }
                        self.rule(positions)?;
                    }
                }
                Some(())
            }
            3 => {
                let mut fields = vec![];
                let mut at = 2;
                for _ in 0..3 {
                    let count = usize::from(table.u16(at)?);
                    fields.extend((0..count).map(|i| at + 2 + 2 * i));
                    at += 2 + 2 * count;
                }
                let positions = fields
                    .into_iter()
                    .map(|field| Some(Position::Glyphs(coverage(table.follow(field)?)?)))
                    .collect::<Option<Vec<_>>>()?;
                self.rule(positions)
            }
            _ => None,
        }
    }
}

/// The glyphs of a face whose clusters compose (see the module docs).
#[derive(Clone, Debug, Default)]
pub(crate) struct LayoutGate {
    composable: GlyphSet,
}

impl LayoutGate {
    /// A gate no glyph passes.
    pub(crate) fn closed() -> Self {
        Self::default()
    }

    /// The glyphs of `initial` that compose, given the face's GSUB and GPOS
    /// tables (empty when it has none) and the features it is shaped with.
    ///
    /// The glyphs a run of `initial` glyphs can hold while HarfBuzz shapes
    /// it are their closure under the single-glyph substitutions. While a
    /// rule can match within that closure, one of its positions, the one
    /// with the fewest such glyphs, loses them, with every glyph that a
    /// substitution turns into one of them. What remains no rule can match.
    pub(crate) fn new(gsub: &[u8], gpos: &[u8], features: &Features, initial: GlyphSet) -> Self {
        let mut layout = Layout::default();
        let read = layout
            .read_table(gsub, true, features)
            .and_then(|()| layout.read_table(gpos, false, features));
        if read.is_none() {
            log::debug!("shaping bypass: unreadable or oversized layout tables");
            return Self::closed();
        }
        let mut successors: HashMap<u16, Vec<u16>> = HashMap::new();
        let mut predecessors: HashMap<u16, Vec<u16>> = HashMap::new();
        for &(from, to) in &layout.edges {
            if from != to {
                successors.entry(from).or_default().push(to);
                predecessors.entry(to).or_default().push(from);
            }
        }
        let reach = |from: &GlyphSet, edges: &HashMap<u16, Vec<u16>>| {
            let mut reached = from.clone();
            let mut queue: Vec<u32> = from.iter().collect();
            while let Some(glyph) = queue.pop() {
                let Ok(glyph) = u16::try_from(glyph) else {
                    continue;
                };
                for &next in edges.get(&glyph).into_iter().flatten() {
                    if reached.insert(u32::from(next)) {
                        queue.push(u32::from(next));
                    }
                }
            }
            reached
        };

        let mut composable = initial;
        loop {
            let reachable = reach(&composable, &successors);
            let total = reachable.len();
            let mut class_counts: HashMap<(usize, u16), usize> = HashMap::new();
            let mut count = |position: &Position| -> usize {
                match position {
                    Position::Glyphs(glyphs) => glyphs
                        .iter()
                        .filter(|&&glyph| reachable.contains(u32::from(glyph)))
                        .count(),
                    &Position::Class { def, class } => {
                        *class_counts.entry((def, class)).or_insert_with(|| {
                            let def = &layout.class_defs[def];
                            let classed = |glyphs: &[u16]| {
                                glyphs
                                    .iter()
                                    .filter(|&&glyph| reachable.contains(u32::from(glyph)))
                                    .count()
                            };
                            if class == 0 {
                                let in_classes: usize =
                                    def.members.values().map(|g| classed(g)).sum();
                                total.saturating_sub(in_classes)
                            } else {
                                def.members.get(&class).map_or(0, |g| classed(g))
                            }
                        })
                    }
                    Position::Any => total,
                }
            };
            let mut doomed = GlyphSet::default();
            for rule in &layout.rules {
                let mut fewest: Option<(usize, &Position)> = None;
                for position in rule {
                    let n = count(position);
                    if n == 0 {
                        fewest = None;
                        break;
                    }
                    if fewest.is_none_or(|(least, _)| n < least) {
                        fewest = Some((n, position));
                    }
                }
                let Some((_, position)) = fewest else {
                    continue;
                };
                match position {
                    Position::Glyphs(glyphs) => {
                        for &glyph in glyphs {
                            if reachable.contains(u32::from(glyph)) {
                                doomed.insert(u32::from(glyph));
                            }
                        }
                    }
                    &Position::Class { def, class: 0 } => {
                        let def = &layout.class_defs[def];
                        for glyph in reachable.iter() {
                            if !u16::try_from(glyph).is_ok_and(|glyph| def.class(glyph) != 0) {
                                doomed.insert(glyph);
                            }
                        }
                    }
                    &Position::Class { def, class } => {
                        let members = layout.class_defs[def].members.get(&class);
                        for &glyph in members.into_iter().flatten() {
                            if reachable.contains(u32::from(glyph)) {
                                doomed.insert(u32::from(glyph));
                            }
                        }
                    }
                    Position::Any => {
                        for glyph in reachable.iter() {
                            doomed.insert(glyph);
                        }
                    }
                }
            }
            if doomed.is_empty() {
                return Self { composable };
            }
            for glyph in reach(&doomed, &predecessors).iter() {
                composable.remove(glyph);
            }
        }
    }

    pub(crate) fn contains(&self, glyph: u32) -> bool {
        self.composable.contains(glyph)
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.composable.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.composable.len()
    }
}

/// HarfBuzz's default-ignorable codepoints (hb-unicode.hh): hidden, and
/// merged into a neighbouring cluster when they are a cluster of their own.
fn is_default_ignorable(c: u32) -> bool {
    matches!(
        c,
        0x00AD
            | 0x034F
            | 0x061C
            | 0x17B4..=0x17B5
            | 0x180B..=0x180E
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x206F
            | 0xFE00..=0xFE0F
            | 0xFEFF
            | 0xFFF0..=0xFFF8
            | 0x1D173..=0x1D17A
            | 0xE0000..=0xE0FFF
    )
}

/// Whether HarfBuzz gives `c` a cluster of its own that it lays out the same
/// way wherever it is, unless a lookup matches it with its neighbours.
pub(crate) fn is_cluster_base(c: char) -> bool {
    if matches!(c, ' '..='~') {
        return true;
    }
    let c = c as u32;
    if is_default_ignorable(c)
        || matches!(
            c,
            // FRACTION SLASH turns on frac, numr and dnom around it.
            0x2044
            // NON-BREAKING HYPHEN is drawn as U+2010 when unmapped.
            | 0x2011
            // Continuations of the preceding cluster (hb_set_unicode_props):
            // halfwidth voiced sound marks, emoji modifiers, and regional
            // indicators, which pair up.
            | 0xFF9E..=0xFF9F
            | 0x1F3FB..=0x1F3FF
            | 0x1F1E6..=0x1F1FF
        )
    {
        return false;
    }
    !matches!(
        harfbuzz::unicode_general_category(c),
        HB_UNICODE_GENERAL_CATEGORY_CONTROL
            | HB_UNICODE_GENERAL_CATEGORY_FORMAT
            | HB_UNICODE_GENERAL_CATEGORY_SURROGATE
            | HB_UNICODE_GENERAL_CATEGORY_SPACING_MARK
            | HB_UNICODE_GENERAL_CATEGORY_ENCLOSING_MARK
            | HB_UNICODE_GENERAL_CATEGORY_NON_SPACING_MARK
            | HB_UNICODE_GENERAL_CATEGORY_LINE_SEPARATOR
            | HB_UNICODE_GENERAL_CATEGORY_PARAGRAPH_SEPARATOR
    )
}

/// The script HarfBuzz's buffer takes from `c`, if `c` decides it.
fn decisive_script(c: char) -> Option<hb_script_t> {
    if c.is_ascii() {
        return c.is_ascii_alphabetic().then_some(HB_SCRIPT_LATIN);
    }
    match harfbuzz::unicode_script(c as u32) {
        HB_SCRIPT_COMMON | HB_SCRIPT_INHERITED | HB_SCRIPT_UNKNOWN => None,
        script => Some(script),
    }
}

/// One single-codepoint cluster of a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Cluster {
    /// Byte offset of `base` in the shaped text.
    pub start: u32,
    pub base: char,
    /// U+FE0E or U+FE0F after `base`.
    pub selector: Option<char>,
}

/// A run as HarfBuzz would cluster it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Run {
    /// The script HarfBuzz guesses for the run, `None` when no codepoint
    /// decides it.
    pub script: Option<hb_script_t>,
    pub clusters: Vec<Cluster>,
    /// Printable ASCII only.
    pub ascii: bool,
}

/// `text`, which starts at byte `offset` of the shaped text, as clusters
/// that may compose: `None` if a codepoint joins its neighbours, or the
/// run's script takes a shaper other than HarfBuzz's default one.
pub(crate) fn segment(text: &str, offset: usize) -> Option<Run> {
    let mut run = Run {
        script: None,
        clusters: Vec::with_capacity(text.len()),
        ascii: true,
    };
    let mut chars = text.char_indices().peekable();
    while let Some((index, base)) = chars.next() {
        if !is_cluster_base(base) {
            return None;
        }
        let selector = chars
            .next_if(|&(_, c)| matches!(c, '\u{FE0E}' | '\u{FE0F}'))
            .map(|(_, c)| c);
        run.ascii &= base.is_ascii() && selector.is_none();
        if run.script.is_none() {
            run.script = decisive_script(base);
        }
        run.clusters.push(Cluster {
            start: u32::try_from(offset + index).ok()?,
            base,
            selector,
        });
    }
    match run.script {
        None
        | Some(
            HB_SCRIPT_LATIN | HB_SCRIPT_GREEK | HB_SCRIPT_CYRILLIC | HB_SCRIPT_HAN
            | HB_SCRIPT_HIRAGANA | HB_SCRIPT_KATAKANA | HB_SCRIPT_BOPOMOFO,
        ) => Some(run),
        Some(_) => None,
    }
}

/// What one cluster gets from HarfBuzz in any run, or that it does not
/// compose.
#[derive(Clone, Debug)]
enum Entry {
    /// Cluster offsets are relative to the cluster's start.
    Glyphs(Box<[RawGlyph]>),
    Opaque,
}

/// Bounds the clusters one table keeps besides ASCII.
const MAX_TABLE_CLUSTERS: usize = 8192;
/// Bounds the tables: faces times sizes times scripts.
const MAX_TABLES: usize = 64;

/// One face's clusters at one size, for runs of one script.
struct ClusterTable {
    /// Printable ASCII, U+0020 to U+007E.
    ascii: [Option<Entry>; 95],
    /// Every other cluster, by codepoint and presentation selector.
    clusters: HashMap<(char, Option<char>), Entry>,
}

impl ClusterTable {
    fn new() -> Self {
        Self {
            ascii: std::array::from_fn(|_| None),
            clusters: HashMap::new(),
        }
    }

    fn get(&self, cluster: &Cluster) -> Option<&Entry> {
        match (cluster.base, cluster.selector) {
            (base @ ' '..='~', None) => self.ascii[base as usize - 0x20].as_ref(),
            (base, selector) => self.clusters.get(&(base, selector)),
        }
    }

    fn insert(&mut self, cluster: &Cluster, entry: Entry) {
        match (cluster.base, cluster.selector) {
            (base @ ' '..='~', None) => self.ascii[base as usize - 0x20] = Some(entry),
            (base, selector) => {
                if self.clusters.len() >= MAX_TABLE_CLUSTERS {
                    self.clusters.clear();
                }
                self.clusters.insert((base, selector), entry);
            }
        }
    }
}

/// Which table a run's clusters come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TableKey {
    pub font_idx: usize,
    /// The face's point size, as bits.
    pub size: u64,
    pub dpi: u32,
    pub script: Option<hb_script_t>,
}

/// What to do with a run.
#[derive(Debug)]
pub(crate) enum Plan {
    /// Every cluster was cached: the run's glyphs.
    Composed(Vec<RawGlyph>),
    /// Every cluster composes but some are not cached: shape the run with
    /// HarfBuzz and [`ClusterCache::harvest`] its output.
    Harvest,
    /// Shape the run with HarfBuzz.
    Shape,
}

#[derive(Default)]
pub(crate) struct ClusterCache {
    tables: HashMap<TableKey, ClusterTable>,
}

impl ClusterCache {
    fn table(&mut self, key: TableKey) -> &mut ClusterTable {
        if self.tables.len() >= MAX_TABLES && !self.tables.contains_key(&key) {
            self.tables.clear();
        }
        self.tables.entry(key).or_insert_with(ClusterTable::new)
    }

    /// Decides how `run` is shaped with the face and size of `key`.
    /// `composes` says whether a cluster not cached yet composes.
    pub(crate) fn plan(
        &mut self,
        key: TableKey,
        run: &Run,
        mut composes: impl FnMut(&Cluster) -> bool,
    ) -> Plan {
        let table = self.table(key);
        let mut missing = false;
        for cluster in &run.clusters {
            match table.get(cluster) {
                Some(Entry::Glyphs(_)) => {}
                Some(Entry::Opaque) => return Plan::Shape,
                None if composes(cluster) => missing = true,
                None => {
                    table.insert(cluster, Entry::Opaque);
                    return Plan::Shape;
                }
            }
        }
        if missing {
            return Plan::Harvest;
        }
        let mut glyphs = Vec::with_capacity(run.clusters.len());
        for cluster in &run.clusters {
            if let Some(Entry::Glyphs(entry)) = table.get(cluster) {
                glyphs.extend(entry.iter().map(|glyph| RawGlyph {
                    cluster: glyph.cluster + cluster.start,
                    ..*glyph
                }));
            }
        }
        Plan::Composed(glyphs)
    }

    /// Caches each cluster of `run` from `glyphs`, HarfBuzz's output for the
    /// whole run. Returns false, caching nothing, unless every glyph belongs
    /// to one cluster's start, in order, and every cluster has one.
    pub(crate) fn harvest(&mut self, key: TableKey, run: &Run, glyphs: &[RawGlyph]) -> bool {
        let mut rest = glyphs;
        let mut entries = Vec::with_capacity(run.clusters.len());
        for cluster in &run.clusters {
            let count = rest
                .iter()
                .take_while(|glyph| glyph.cluster == cluster.start)
                .count();
            if count == 0 {
                return false;
            }
            let (own, tail) = rest.split_at(count);
            entries.push(
                own.iter()
                    .map(|glyph| RawGlyph {
                        cluster: 0,
                        ..*glyph
                    })
                    .collect::<Box<[_]>>(),
            );
            rest = tail;
        }
        if !rest.is_empty() {
            return false;
        }
        let table = self.table(key);
        for (cluster, entry) in run.clusters.iter().zip(entries) {
            if table.get(cluster).is_none() {
                table.insert(cluster, Entry::Glyphs(entry));
            }
        }
        true
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn glyph_sets_insert_remove_and_iterate_in_order() {
        let mut set = GlyphSet::default();
        for glyph in [70, 0, 63, 64, 65535, 70] {
            set.insert(glyph);
        }
        set.remove(63);
        assert!(set.contains(65535) && set.contains(0) && !set.contains(63));
        assert_eq!(set.iter().collect::<Vec<_>>(), vec![0, 64, 70, 65535]);
        assert_eq!(set.len(), 4);
    }

    #[test]
    fn segmentation_follows_harfbuzz_clusters_and_script() {
        let run = segment("a\u{1F600}\u{FE0F}b", 3).unwrap();
        assert_eq!(run.script, Some(HB_SCRIPT_LATIN));
        assert!(!run.ascii);
        assert_eq!(
            run.clusters,
            vec![
                Cluster {
                    start: 3,
                    base: 'a',
                    selector: None
                },
                Cluster {
                    start: 4,
                    base: '\u{1F600}',
                    selector: Some('\u{FE0F}')
                },
                Cluster {
                    start: 11,
                    base: 'b',
                    selector: None
                },
            ]
        );
        assert_eq!(segment("12 +", 0).unwrap().script, None);
        assert!(segment("12 +", 0).unwrap().ascii);
        assert_eq!(
            segment("1\u{4E00}a", 0).unwrap().script,
            Some(HB_SCRIPT_HAN)
        );
        // Joiners, marks, modifiers, regional indicators, the fraction
        // slash, a second selector, and scripts with their own shapers.
        for text in [
            "\u{1F468}\u{200D}\u{1F469}",
            "e\u{301}",
            "\u{1F44B}\u{1F3FB}",
            "\u{1F1FA}\u{1F1F8}",
            "1\u{2044}2",
            "\u{2764}\u{FE0F}\u{FE0F}",
            "\u{FE0F}",
            "\u{627}",
            "\u{AC00}",
            "\u{5D0}a",
        ] {
            assert_eq!(segment(text, 0), None, "{text:?}");
        }
    }

    fn be16(values: &[u16]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_be_bytes()).collect()
    }

    /// A GSUB table with a `liga` ligature lookup forming glyph 10 from
    /// glyphs 1 and 2, and a `smcp` single substitution adding `delta` to
    /// glyph 3.
    fn gsub_with_ligature(delta: u16) -> Vec<u8> {
        // Header: ScriptList at 10, FeatureList at 32, LookupList at 58.
        let mut data = be16(&[1, 0, 10, 32, 58]);
        // ScriptList: one script, 'DFLT', with its Script table at +8.
        data.extend(be16(&[1]));
        data.extend(b"DFLT");
        data.extend(be16(&[8]));
        // Script: default LangSys at +4, no other language systems.
        data.extend(be16(&[4, 0]));
        // LangSys: no required feature; features 0 and 1.
        data.extend(be16(&[0, 0xFFFF, 2, 0, 1]));
        assert_eq!(data.len(), 32);
        // FeatureList: 'liga' -> lookup 0 at +14, 'smcp' -> lookup 1 at +20.
        data.extend(be16(&[2]));
        data.extend(b"liga");
        data.extend(be16(&[14]));
        data.extend(b"smcp");
        data.extend(be16(&[20]));
        data.extend(be16(&[0, 1, 0]));
        data.extend(be16(&[0, 1, 1]));
        assert_eq!(data.len(), 58);
        // LookupList: lookup 0 at +6; lookup 1 after lookup 0's 8 bytes, its
        // subtable's 8, coverage's 6, ligature set's 4 and ligature's 6.
        data.extend(be16(&[2, 6, 6 + 8 + 8 + 6 + 4 + 6]));
        // Lookup 0: ligature substitution, one subtable at +8.
        data.extend(be16(&[4, 0, 1, 8]));
        // Format 1, coverage at +8, one ligature set at +14.
        data.extend(be16(&[1, 8, 1, 14]));
        // Coverage format 1: glyph 1.
        data.extend(be16(&[1, 1, 1]));
        // LigatureSet: one ligature at +4.
        data.extend(be16(&[1, 4]));
        // Ligature: glyph 10 from 1 and 2.
        data.extend(be16(&[10, 2, 2]));
        // Lookup 1: single substitution, one subtable at +8.
        data.extend(be16(&[1, 0, 1, 8]));
        // Format 1, coverage at +6, `delta`; coverage format 1: glyph 3.
        data.extend(be16(&[1, 6, delta]));
        data.extend(be16(&[1, 1, 3]));
        data
    }

    fn glyphs(set: impl IntoIterator<Item = u32>) -> GlyphSet {
        let mut glyphs = GlyphSet::default();
        for glyph in set {
            glyphs.insert(glyph);
        }
        glyphs
    }

    fn feature(name: &[u8; 4], value: u32) -> harfbuzz::hb_feature_t {
        harfbuzz::hb_feature_t {
            tag: tag(name),
            value,
            start: 0,
            end: u32::MAX,
        }
    }

    #[test]
    fn a_reachable_ligature_takes_a_component_out_of_the_gate() {
        let gsub = gsub_with_ligature(1);
        let features = Features::applied(&[]).unwrap();
        // 1 and 2 can form the ligature: its first position, 1, goes.
        let gate = LayoutGate::new(&gsub, &[], &features, glyphs([0, 1, 2, 3, 5]));
        assert_eq!(gate.composable, glyphs([0, 2, 3, 5]));

        // Without both components nothing can match.
        let gate = LayoutGate::new(&gsub, &[], &features, glyphs([0, 1, 3]));
        assert_eq!(gate.composable, glyphs([0, 1, 3]));

        // liga=0 turns the ligature off.
        let off = Features::applied(&[feature(b"liga", 0)]).unwrap();
        let gate = LayoutGate::new(&gsub, &[], &off, glyphs([1, 2]));
        assert_eq!(gate.composable, glyphs([1, 2]));
    }

    #[test]
    fn a_glyph_substituted_into_a_ligature_component_goes_with_it() {
        // smcp turns 3 into 1, the ligature's first component.
        let gsub = gsub_with_ligature(0xFFFE);
        let smcp = Features::applied(&[feature(b"smcp", 1)]).unwrap();
        let gate = LayoutGate::new(&gsub, &[], &smcp, glyphs([2, 3]));
        assert_eq!(gate.composable, glyphs([2]));
        // smcp is off by default: 3 stays 3.
        let features = Features::applied(&[]).unwrap();
        let gate = LayoutGate::new(&gsub, &[], &features, glyphs([2, 3]));
        assert_eq!(gate.composable, glyphs([2, 3]));
    }

    #[test]
    fn partial_range_features_and_malformed_tables_close_the_gate() {
        let mut ranged = feature(b"liga", 1);
        ranged.end = 3;
        assert!(Features::applied(&[ranged]).is_none());
        let features = Features::applied(&[]).unwrap();
        let mut gsub = gsub_with_ligature(1);
        gsub.truncate(70);
        assert!(LayoutGate::new(&gsub, &[], &features, glyphs([1])).is_closed());
    }
}
