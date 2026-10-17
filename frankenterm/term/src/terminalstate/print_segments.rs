//! The print path's grapheme segmentation, with a per-character fast path
//! (ft-yccm0.3.2.6).
//!
//! `finl_unicode`'s `Graphemes` starts a fresh cluster state machine at
//! every boundary. A character whose first step leaves that machine in its
//! plain state (grapheme property Other, and not CR) is a cluster of its
//! own whenever the character after it does not continue a cluster
//! (Extend, SpacingMark, ZWJ, InCB Linker), since in the plain state the
//! machine breaks before every other character; and the text's last
//! character, whatever it is, is a cluster of its own. Both facts are read from
//! the machine itself: [`class_block`] runs `Graphemes` over probe strings
//! for each character, so the classes agree with it by construction, and a
//! cluster the fast path does not settle is taken from `Graphemes` started
//! at the cluster's first character, exactly where `Graphemes` itself
//! starts it. On mixed-script text most clusters are a single such
//! character, and segmenting them was a fifth of the ingest. A plain
//! character followed by continuing characters that each leave the machine
//! plain again (a letter and its combining marks) is settled the same way:
//! the cluster ends before the first character that does not continue it.
//!
//! [`PrintCharTable`] also keeps each character's `grapheme_column_width`
//! under the terminal's Unicode version, so the print path looks a lone
//! character up once for both.

use finl_unicode::grapheme_clusters::Graphemes;
use frankenterm_cell::{grapheme_column_width, UnicodeVersion};
use std::convert::TryFrom;
use std::sync::{Arc, OnceLock};

/// Starts a cluster in the machine's plain state.
const PLAIN: u8 = 0b01;
/// Continues the cluster before it.
const CONTINUES: u8 = 0b10;
/// Continues a cluster and, after a plain character, leaves the machine in
/// its plain state again (a combining mark after a letter, say). (Bit 2 is
/// [`WIDTH_KNOWN`]'s.)
const RESUMES: u8 = 0b1000;

/// Blocks of 256 characters in the code space.
const BLOCKS: usize = 0x11_0000 >> 8;

/// The classes of block `block`'s characters (surrogates are none),
/// computed once per process.
fn class_block(block: usize) -> &'static [u8; 256] {
    static CLASSES: [OnceLock<[u8; 256]>; BLOCKS] = [const { OnceLock::new() }; BLOCKS];
    CLASSES[block].get_or_init(|| {
        let mut classes = [0; 256];
        let mut probe = String::with_capacity(16);
        for (low, class) in classes.iter_mut().enumerate() {
            if let Some(c) = char::from_u32(((block << 8) | low) as u32) {
                *class = classify(c, &mut probe);
            }
        }
        classes
    })
}

/// Followers the plain state splits from and some other state joins.
const SPLIT_FROM_PLAIN: [&str; 8] = [
    "a",
    "\u{1f642}",
    "\u{1f1e6}",
    "\u{1100}",
    "\u{1161}",
    "\u{ac00}",
    "\u{11a8}",
    "\u{915}",
];

/// The clusters `Graphemes` finds in `before`, `c`, `after`.
fn clusters(probe: &mut String, before: &str, c: char, after: &str) -> usize {
    probe.clear();
    probe.push_str(before);
    probe.push(c);
    probe.push_str(after);
    Graphemes::new(probe.as_str()).count()
}

/// `c`'s class, read off `Graphemes`. After a plain character (`a`) the
/// machine joins exactly the continuing characters. A plain first step is
/// told from every other one by a follower that the other state treats
/// differently: a control ends its cluster at once (so a combining mark
/// does not join it), Prepend takes anything after it, an emoji joins
/// ZWJ and a second emoji, a regional indicator a second one, Hangul L a
/// V, Hangul V, LV, T and LVT a T, and an InCB consonant a linker and a
/// second consonant. CR, which pairs with LF, is never plain.
///
/// A continuing character resumes the plain state when, after `a` and it,
/// the machine still joins a continuing character and splits before every
/// follower that some other state would join: another character, an
/// emoji, a regional indicator, Hangul L, V, LV and T, and an InCB
/// consonant. Every plain character leaves the machine in the same state
/// as `a`, so this holds after any plain character, and after any run of
/// resuming characters.
fn classify(c: char, probe: &mut String) -> u8 {
    let continues = clusters(probe, "a", c, "") == 1;
    let plain = c != '\r'
        && !continues
        && clusters(probe, "", c, "\u{301}") == 1
        && clusters(probe, "", c, "a") == 2
        && clusters(probe, "", c, "\u{200d}\u{1f642}") == 2
        && clusters(probe, "", c, "\u{1f1e6}") == 2
        && clusters(probe, "", c, "\u{1161}") == 2
        && clusters(probe, "", c, "\u{11a8}") == 2
        && clusters(probe, "", c, "\u{94d}\u{915}") == 2;
    let resumes = continues
        && clusters(probe, "a", c, "\u{301}") == 1
        && SPLIT_FROM_PLAIN
            .iter()
            .all(|follower| clusters(probe, "a", c, follower) == 2);
    (if plain { PLAIN } else { 0 })
        | (if continues { CONTINUES } else { 0 })
        | (if resumes { RESUMES } else { 0 })
}

/// An entry of [`PrintCharTable`]: the class in the low byte and, when
/// `WIDTH_KNOWN` is set, the character's column width in the high byte.
const WIDTH_KNOWN: u16 = 0b100;

/// Per-terminal character classes and widths for [`PrintSegments`], filled
/// a block at a time on first use and refilled when the Unicode version
/// changes.
#[derive(Default)]
pub(crate) struct PrintCharTable {
    version: Option<UnicodeVersion>,
    blocks: Vec<Option<Box<[u16; 256]>>>,
}

impl std::fmt::Debug for PrintCharTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrintCharTable")
            .field("version", &self.version.as_ref().map(|v| v.version))
            .field(
                "blocks",
                &self.blocks.iter().filter(|block| block.is_some()).count(),
            )
            .finish()
    }
}

/// Whether widths computed under `a` hold under `b`: the same version and
/// ambiguous-width choice, and the same custom width map (compared by
/// identity, which the table's own copy keeps alive).
fn same_widths(a: &UnicodeVersion, b: &UnicodeVersion) -> bool {
    a.version == b.version
        && a.ambiguous_are_wide == b.ambiguous_are_wide
        && match (&a.cell_widths, &b.cell_widths) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
}

impl PrintCharTable {
    /// The graphemes of `text`, with the column width of each lone
    /// character the fast path settles, under `version`.
    pub(crate) fn segments<'a, 't>(
        &'t mut self,
        text: &'a str,
        version: &UnicodeVersion,
    ) -> PrintSegments<'a, 't> {
        if !self
            .version
            .as_ref()
            .is_some_and(|known| same_widths(known, version))
        {
            self.version = Some(version.clone());
            self.blocks.clear();
        }
        PrintSegments {
            text,
            pos: 0,
            table: self,
        }
    }

    #[inline]
    fn entry(&mut self, c: char) -> u16 {
        let block = c as usize >> 8;
        match self.blocks.get(block) {
            Some(Some(entries)) => entries[c as usize & 0xff],
            _ => self.fill(block)[c as usize & 0xff],
        }
    }

    #[cold]
    #[inline(never)]
    fn fill(&mut self, block: usize) -> &[u16; 256] {
        let classes = class_block(block);
        let version = self.version.as_ref();
        let mut entries = Box::new([0_u16; 256]);
        let mut utf8 = [0_u8; 4];
        for (low, entry) in entries.iter_mut().enumerate() {
            let Some(c) = char::from_u32(((block << 8) | low) as u32) else {
                continue;
            };
            let class = u16::from(classes[low]);
            let width = grapheme_column_width(c.encode_utf8(&mut utf8), version);
            *entry = match u8::try_from(width) {
                Ok(width) => class | WIDTH_KNOWN | u16::from(width) << 8,
                Err(_) => class,
            };
        }
        if self.blocks.len() <= block {
            self.blocks.resize_with(block + 1, || None);
        }
        self.blocks[block].insert(entries)
    }
}

/// The graphemes of a print buffer, as `Graphemes` yields them, each with
/// its column width when it is a lone character the table knows.
pub(crate) struct PrintSegments<'a, 't> {
    text: &'a str,
    pos: usize,
    table: &'t mut PrintCharTable,
}

impl<'a> Iterator for PrintSegments<'a, '_> {
    type Item = (&'a str, Option<usize>);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let rest = self.text.get(self.pos..)?;
        let mut chars = rest.chars();
        let c = chars.next()?;
        let entry = self.table.entry(c);
        // A last character is a cluster of its own whatever its class (one
        // scalar value is always one extended grapheme cluster): T0 flushes
        // a lone emoji between every SGR pair.
        let alone = match chars.next() {
            None => true,
            Some(next) => {
                u16::from(PLAIN) & entry != 0 && u16::from(CONTINUES) & self.table.entry(next) == 0
            }
        };
        if alone {
            let len = c.len_utf8();
            self.pos += len;
            let width = (entry & WIDTH_KNOWN != 0).then(|| usize::from(entry >> 8));
            return Some((&rest[..len], width));
        }
        if u16::from(PLAIN) & entry != 0 {
            if let Some(len) = self.plain_cluster_len(rest, c.len_utf8()) {
                self.pos += len;
                return Some((&rest[..len], None));
            }
        }
        let grapheme = Graphemes::new(rest).next()?;
        self.pos += grapheme.len();
        Some((grapheme, None))
    }
}

impl<'a> PrintSegments<'a, '_> {
    /// Takes the run of graphemes at the front of what is left that
    /// [`Iterator::next`] would yield as lone characters of a known width,
    /// one or two columns, while all of them fit in fewer than `room`
    /// columns, and only one-byte ones without `multibyte`: each goes to
    /// `cells` with its width, exactly as `next` yields it. Stops before
    /// the first that is not, which `next` yields next. Returns the columns
    /// taken.
    ///
    /// The print loop's fast path asked of each such grapheme what is
    /// settled for the whole run once it has printed one (ft-yccm0.3.2.6),
    /// and `next` decoded every character twice (as the lookahead, then as
    /// itself); here each is decoded and looked up once.
    pub(crate) fn plain_run(
        &mut self,
        room: usize,
        multibyte: bool,
        cells: &mut Vec<(&'a str, u8)>,
    ) -> usize {
        let text = self.text;
        let Some(rest) = text.get(self.pos..) else {
            return 0;
        };
        let mut chars = rest.chars();
        let Some(mut c) = chars.next() else {
            return 0;
        };
        let mut entry = self.table.entry(c);
        let mut pos = self.pos;
        let mut used = 0;
        loop {
            let next = chars.next();
            let next_entry = next.map(|next| self.table.entry(next));
            // `next`'s test of a lone character.
            let alone = next_entry
                .is_none_or(|next| u16::from(PLAIN) & entry != 0 && u16::from(CONTINUES) & next == 0);
            let width = usize::from(entry >> 8);
            let len = c.len_utf8();
            if !alone
                || entry & WIDTH_KNOWN == 0
                || !(1..=2).contains(&width)
                || used + width >= room
                || (len > 1 && !multibyte)
            {
                break;
            }
            cells.push((&text[pos..pos + len], width as u8));
            pos += len;
            used += width;
            match (next, next_entry) {
                (Some(next), Some(next_entry)) => {
                    c = next;
                    entry = next_entry;
                }
                _ => break,
            }
        }
        self.pos = pos;
        used
    }

    /// The length of the cluster that the plain character `first` bytes
    /// long at the front of `rest` starts, when each character continuing
    /// it resumes the plain state: it then ends before the first character
    /// that does not continue it (a letter and its combining marks, say).
    /// `None` when a continuing character does not resume it.
    #[inline]
    fn plain_cluster_len(&mut self, rest: &str, first: usize) -> Option<usize> {
        let mut len = first;
        for k in rest[first..].chars() {
            let entry = self.table.entry(k);
            if u16::from(CONTINUES) & entry == 0 {
                break;
            }
            if u16::from(RESUMES) & entry == 0 {
                return None;
            }
            len += k.len_utf8();
        }
        Some(len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn latest() -> UnicodeVersion {
        UnicodeVersion::new(16)
    }

    fn segmented(table: &mut PrintCharTable, text: &str) -> Vec<(String, Option<usize>)> {
        table
            .segments(text, &latest())
            .map(|(g, width)| (g.to_string(), width))
            .collect()
    }

    fn reference(text: &str) -> Vec<String> {
        Graphemes::new(text).map(str::to_string).collect()
    }

    /// The probes classify by the properties the module docs name; these
    /// pairs pin that each probe character has them in `Graphemes`.
    #[test]
    fn the_probe_characters_behave_as_classify_assumes() {
        for joined in [
            "a\u{301}",
            "\u{600}a",
            "\u{1f642}\u{200d}\u{1f642}",
            "\u{1f1e6}\u{1f1e6}",
            "\u{1100}\u{1161}",
            "\u{ac00}\u{11a8}",
            "\u{11a8}\u{11a8}",
            "\u{915}\u{94d}\u{915}",
            "\r\n",
        ] {
            assert_eq!(reference(joined).len(), 1, "{:?}", joined.escape_unicode());
        }
        for apart in ["ab", "a\u{1f642}", "a\u{1f1e6}", "a\u{1161}", "a\u{11a8}"] {
            assert_eq!(reference(apart).len(), 2, "{:?}", apart.escape_unicode());
        }
    }

    /// Characters before one of each grapheme property: the fast path splits
    /// a pair exactly where `Graphemes` does, which covers every decision it
    /// makes on its own, and a lone character's width is
    /// `grapheme_column_width`'s. Every character of planes 0, 1 and 14,
    /// where nearly all the properties other than Other live, and every
    /// 17th block of 256 elsewhere (a block's classes are computed whole).
    #[test]
    fn characters_pair_as_graphemes_pairs_them() {
        let followers: Vec<char> = concat!(
            "a\u{301}\u{903}\u{200d}\n\r\u{1}\u{600}\u{1f642}\u{1f1e6}",
            "\u{1100}\u{1161}\u{11a8}\u{ac00}\u{ac01}\u{915}\u{94d}\u{fe0f}\u{e0020}",
        )
        .chars()
        .collect();
        let sampled = |cp: u32| {
            cp < 0x2_0000 || (0xe_0000..0xf_0000).contains(&cp) || (cp >> 8).is_multiple_of(17)
        };
        let mut table = PrintCharTable::default();
        let mut text = String::new();
        for c in (0..=0x10_ffff_u32)
            .filter(|&cp| sampled(cp))
            .filter_map(char::from_u32)
        {
            for &follower in &followers {
                text.clear();
                text.push(c);
                text.push(follower);
                let mut expected = Graphemes::new(&text);
                for (g, width) in table.segments(&text, &latest()) {
                    assert_eq!(Some(g), expected.next(), "{:?}", text.escape_unicode());
                    if let Some(width) = width {
                        assert_eq!(width, grapheme_column_width(g, Some(&latest())));
                    }
                }
                assert_eq!(expected.next(), None, "{:?}", text.escape_unicode());
            }
        }
    }

    /// A plain character, one or two of a continuing character that
    /// resumes the plain state, and a follower of each grapheme property:
    /// the fast path ends the cluster exactly where `Graphemes` does, for
    /// every such character of planes 0, 1 and 14 after plain characters of
    /// several scripts.
    #[test]
    fn resuming_characters_end_clusters_as_graphemes_ends_them() {
        let bases = [
            'a', '\u{e9}', '\u{4e2d}', '\u{5d0}', '\u{645}', '\u{3042}', '\u{2500}',
        ];
        let followers: Vec<char> = concat!(
            "a\u{301}\u{903}\u{200d}\n\r\u{1}\u{600}\u{1f642}\u{1f1e6}",
            "\u{1100}\u{1161}\u{11a8}\u{ac00}\u{ac01}\u{915}\u{94d}\u{fe0f}\u{e0020}",
        )
        .chars()
        .collect();
        let mut table = PrintCharTable::default();
        for &base in &bases {
            assert_ne!(u16::from(PLAIN) & table.entry(base), 0, "{:?}", base);
        }
        let mut text = String::new();
        let mut resuming = 0;
        for k in (0..0x2_0000_u32)
            .chain(0xe_0000..0xf_0000)
            .filter_map(char::from_u32)
        {
            if u16::from(RESUMES) & table.entry(k) == 0 {
                continue;
            }
            resuming += 1;
            for &base in &bases {
                for repeat in 1..=2 {
                    for &follower in &followers {
                        text.clear();
                        text.push(base);
                        for _ in 0..repeat {
                            text.push(k);
                        }
                        text.push(follower);
                        let segments: Vec<(&str, Option<usize>)> =
                            table.segments(&text, &latest()).collect();
                        let fast: Vec<&str> = segments.iter().map(|&(g, _)| g).collect();
                        let expected: Vec<&str> = Graphemes::new(&text).collect();
                        assert_eq!(fast, expected, "{:?}", text.escape_unicode());
                        for (g, width) in segments {
                            if let Some(width) = width {
                                assert_eq!(
                                    width,
                                    grapheme_column_width(g, Some(&latest())),
                                    "{:?}",
                                    g.escape_unicode()
                                );
                            }
                        }
                    }
                }
            }
        }
        // The combining marks alone are well over a thousand.
        assert!(resuming > 1000, "{} resuming characters", resuming);
    }

    fn mixed_char() -> impl Strategy<Value = char> {
        let tricky: Vec<char> = concat!(
            "aZ ~#\u{e9}\u{301}\u{308}\u{903}\u{200d}\u{200b}\u{fe0f}\u{fe0e}\u{20e3}",
            "\u{600}\u{5d0}\u{645}\u{64b}\u{4e2d}\u{3042}\u{1100}\u{1161}\u{11a8}",
            "\u{ac00}\u{ac01}\u{915}\u{94d}\u{93f}\u{1f468}\u{1f469}\u{1f467}\u{1f3fb}",
            "\u{2764}\u{1f1fa}\u{1f1f8}\u{1f642}\u{e000}\u{2500}\u{e0061}\u{e007f}\u{1f3f4}",
        )
        .chars()
        .collect();
        prop_oneof![prop::sample::select(tricky), any::<char>()]
    }

    proptest! {
        /// Whole buffers segment exactly as `Graphemes` segments them.
        #[test]
        fn buffers_segment_as_graphemes_segments_them(
            chars in prop::collection::vec(mixed_char(), 0..48)
        ) {
            let text: String = chars.into_iter().collect();
            let mut table = PrintCharTable::default();
            let graphemes: Vec<String> =
                segmented(&mut table, &text).into_iter().map(|(g, _)| g).collect();
            prop_assert_eq!(graphemes, reference(&text));
        }
    }

    /// A new Unicode version refills the widths.
    #[test]
    fn widths_follow_the_unicode_version() {
        let mut table = PrintCharTable::default();
        let mut narrow = latest();
        narrow.ambiguous_are_wide = false;
        let mut wide = latest();
        wide.ambiguous_are_wide = true;
        // U+00B0 DEGREE SIGN has East Asian Width Ambiguous.
        let width = |table: &mut PrintCharTable, version: &UnicodeVersion| {
            table
                .segments("\u{b0}", version)
                .next()
                .and_then(|(_, w)| w)
        };
        assert_eq!(width(&mut table, &narrow), Some(1));
        assert_eq!(width(&mut table, &wide), Some(2));
        assert_eq!(width(&mut table, &narrow), Some(1));
    }
}
