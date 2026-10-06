//! The page grapheme arena (PageGrid ADR section 3.5).
//!
//! A cell with its `grapheme` bit keeps its first scalar inline; the arena
//! keeps the complete UTF-8, keyed by the cell offset. Blocks come in size
//! classes of 16, 32 and 64 bytes, with longer graphemes stored exactly.
//! Freed class blocks are reused through per-class free lists, and the
//! arena compacts when free space exceeds half of it. Growth is `Vec`
//! growth: nothing is cloned.

use super::hash::FxHashMap;

/// Arena bytes a reset page keeps at most.
pub const STD_ARENA_BYTES: usize = 8 * 1024;
/// Map entries a reset page keeps room for at most.
pub const STD_ARENA_ENTRIES: usize = 256;
/// The map stores lengths as `u16`, so a longer cluster keeps its first
/// 64 KiB, cut at a scalar boundary. The parser has no grapheme-length cap;
/// this one also bounds what a hostile stream can pin per cell.
pub const MAX_GRAPHEME_BYTES: usize = u16::MAX as usize;

const CLASSES: [usize; 3] = [16, 32, 64];

fn block_size(len: usize) -> usize {
    CLASSES
        .iter()
        .copied()
        .find(|&class| len <= class)
        .unwrap_or(len)
}

fn class_of(size: usize) -> Option<usize> {
    CLASSES.iter().position(|&class| class == size)
}

/// `text` cut to at most [`MAX_GRAPHEME_BYTES`] at a scalar boundary.
pub fn clamp_grapheme(text: &str) -> &str {
    if text.len() <= MAX_GRAPHEME_BYTES {
        return text;
    }
    let mut end = MAX_GRAPHEME_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[derive(Clone, Debug, Default)]
pub struct GraphemeArena {
    bytes: Vec<u8>,
    /// Cell offset to `(byte offset, byte length)`.
    map: FxHashMap<u32, (u32, u16)>,
    /// Free block offsets per size class.
    free: [Vec<u32>; 3],
    /// Bytes in live blocks (class-rounded).
    live_bytes: usize,
}

impl GraphemeArena {
    /// An empty arena; nothing is allocated until the first grapheme.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Arena size in bytes, free blocks included.
    pub fn arena_bytes(&self) -> usize {
        self.bytes.len()
    }

    pub fn contains(&self, key: u32) -> bool {
        self.map.contains_key(&key)
    }

    pub fn get(&self, key: u32) -> Option<&str> {
        let &(offset, len) = self.map.get(&key)?;
        let start = offset as usize;
        std::str::from_utf8(&self.bytes[start..start + len as usize]).ok()
    }

    fn alloc(&mut self, len: usize) -> usize {
        let size = block_size(len);
        self.live_bytes += size;
        if let Some(offset) = class_of(size).and_then(|class| self.free[class].pop()) {
            return offset as usize;
        }
        let offset = self.bytes.len();
        debug_assert!(offset + size <= u32::MAX as usize);
        self.bytes.resize(offset + size, 0);
        offset
    }

    /// Stores `text` for the cell at `key`, which must not hold one.
    pub fn insert(&mut self, key: u32, text: &str) {
        let text = clamp_grapheme(text);
        let offset = self.alloc(text.len());
        self.bytes[offset..offset + text.len()].copy_from_slice(text.as_bytes());
        let previous = self.map.insert(key, (offset as u32, text.len() as u16));
        debug_assert!(previous.is_none(), "cell {} already has a grapheme", key);
    }

    /// Stores a copy of `from`'s grapheme for the cell at `to`.
    pub fn duplicate(&mut self, from: u32, to: u32) {
        let Some(&(source, len)) = self.map.get(&from) else {
            debug_assert!(false, "cell {} has no grapheme to copy", from);
            return;
        };
        let offset = self.alloc(len as usize);
        let source = source as usize;
        self.bytes
            .copy_within(source..source + len as usize, offset);
        let previous = self.map.insert(to, (offset as u32, len));
        debug_assert!(previous.is_none(), "cell {} already has a grapheme", to);
    }

    /// Frees the cell's grapheme; false when it has none.
    pub fn remove(&mut self, key: u32) -> bool {
        let Some((offset, len)) = self.map.remove(&key) else {
            return false;
        };
        let size = block_size(len as usize);
        self.live_bytes -= size;
        if let Some(class) = class_of(size) {
            self.free[class].push(offset);
        }
        if self.bytes.len() > STD_ARENA_BYTES
            && (self.bytes.len() - self.live_bytes) * 2 > self.bytes.len()
        {
            self.compact();
        }
        true
    }

    /// Detaches the cell's entry without freeing it, for re-keying. Every
    /// detached entry must be [`Self::put`] back before the next
    /// [`Self::remove`]: compaction only keeps blocks the map names.
    pub(crate) fn take(&mut self, key: u32) -> Option<(u32, u16)> {
        self.map.remove(&key)
    }

    /// Re-attaches an entry from [`Self::take`] under a new key.
    pub(crate) fn put(&mut self, key: u32, entry: (u32, u16)) {
        let previous = self.map.insert(key, entry);
        debug_assert!(previous.is_none(), "cell {} already has a grapheme", key);
    }

    /// Repacks the live blocks; keys are unchanged.
    fn compact(&mut self) {
        let mut bytes = Vec::with_capacity(self.live_bytes.max(STD_ARENA_BYTES));
        for (offset, len) in self.map.values_mut() {
            let start = *offset as usize;
            let new_offset = bytes.len();
            bytes.extend_from_slice(&self.bytes[start..start + *len as usize]);
            bytes.resize(new_offset + block_size(*len as usize), 0);
            *offset = new_offset as u32;
        }
        self.bytes = bytes;
        self.free.iter_mut().for_each(Vec::clear);
    }

    pub fn reset(&mut self) {
        self.bytes.clear();
        self.bytes.shrink_to(STD_ARENA_BYTES);
        self.map.clear();
        self.map.shrink_to(STD_ARENA_ENTRIES);
        for list in &mut self.free {
            list.clear();
            list.shrink_to(0);
        }
        self.live_bytes = 0;
    }

    /// Empty and within standard capacity (I14).
    pub fn is_clean(&self) -> bool {
        let std_map_capacity = FxHashMap::<u32, (u32, u16)>::with_capacity_and_hasher(
            STD_ARENA_ENTRIES,
            Default::default(),
        )
        .capacity();
        self.bytes.is_empty()
            && self.bytes.capacity() <= STD_ARENA_BYTES
            && self.map.is_empty()
            && self.map.capacity() <= std_map_capacity
            && self.free.iter().all(|list| list.capacity() == 0)
            && self.live_bytes == 0
    }

    /// Every key, for the page's I6 and I8 checks.
    pub(crate) fn keys(&self) -> impl Iterator<Item = u32> + '_ {
        self.map.keys().copied()
    }

    /// I6, arena half: blocks are in bounds, disjoint (free blocks
    /// included) and valid UTF-8.
    pub(crate) fn check(&self) -> Result<(), String> {
        let mut blocks: Vec<(usize, usize, bool)> = Vec::with_capacity(self.map.len());
        let mut live_bytes = 0;
        for (&key, &(offset, len)) in &self.map {
            let start = offset as usize;
            let size = block_size(len as usize);
            if start + size > self.bytes.len() {
                return Err(format!("grapheme of cell {} runs past the arena", key));
            }
            if std::str::from_utf8(&self.bytes[start..start + len as usize]).is_err() {
                return Err(format!("grapheme of cell {} is not UTF-8", key));
            }
            live_bytes += size;
            blocks.push((start, size, true));
        }
        for (class, list) in self.free.iter().enumerate() {
            for &offset in list {
                let start = offset as usize;
                if start + CLASSES[class] > self.bytes.len() {
                    return Err(format!("free block at {} runs past the arena", start));
                }
                blocks.push((start, CLASSES[class], false));
            }
        }
        if live_bytes != self.live_bytes {
            return Err(format!(
                "live bytes {} but blocks hold {}",
                self.live_bytes, live_bytes
            ));
        }
        blocks.sort_unstable();
        for pair in blocks.windows(2) {
            if pair[0].0 + pair[0].1 > pair[1].0 {
                return Err(format!(
                    "arena blocks at {} and {} overlap",
                    pair[0].0, pair[1].0
                ));
            }
        }
        Ok(())
    }
}
