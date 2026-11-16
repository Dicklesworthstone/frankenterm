//! The per-page rich-style table (PageGrid ADR D3 and section 3.4).
//!
//! Only styles with true colour or an underline colour live here; every
//! palette style is inline in the cell (D2). Ids are `u32` positions in an
//! entry arena (0 is never an id). A separate index of id slots, probed
//! linearly, finds an id by value. It starts at 512 slots and only the index
//! is rehashed when it fills: cells keep their ids, and the page is never
//! cloned or split.

use super::hash::FxHasher;
use crate::color::ColorAttribute;
use std::hash::{Hash, Hasher};

/// Index slots in a new or reset table.
pub const STD_INDEX_SLOTS: usize = 512;

/// A style the inline cell form cannot hold. The colours stay legacy
/// `ColorAttribute` values so materialization is exact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RichStyle {
    /// The 14 inline attribute bits (see `cell::encode_attrs`).
    pub attrs: u16,
    pub fg: ColorAttribute,
    pub bg: ColorAttribute,
    pub underline_color: ColorAttribute,
}

impl RichStyle {
    fn hash32(&self) -> u32 {
        let mut hasher = FxHasher::default();
        self.hash(&mut hasher);
        // The high half of the product mixes every input bit.
        (hasher.finish() >> 32) as u32
    }
}

/// The pen's one-entry cache of its last rich id: the hook for the B2.4
/// last-SGR cache. It holds no reference. [`RichStyleTable::acquire`]
/// accepts it only for the same page incarnation (serials are never reused,
/// D6), the same entry generation and an equal style, so a stale or foreign
/// hint costs one comparison and falls back to the hashed lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CachedRichId {
    pub page_serial: u64,
    pub id: u32,
    pub generation: u32,
}

#[derive(Clone, Debug)]
struct RichEntry {
    style: RichStyle,
    hash: u32,
    /// 0 means the entry is free.
    refs: u32,
    /// Bumped each time the entry is freed.
    generation: u32,
}

#[derive(Clone, Debug)]
pub struct RichStyleTable {
    /// Entry for id `k` is `entries[k - 1]`. Allocated on first use.
    entries: Vec<RichEntry>,
    free: Vec<u32>,
    /// Power-of-two slots; 0 is empty, anything else is a live id.
    index: Box<[u32]>,
    live: u32,
    /// Hashed lookups since the last reset, for tests and benches that
    /// check the hot path never hashes.
    hashes: u64,
}

impl Default for RichStyleTable {
    fn default() -> Self {
        Self::new()
    }
}

impl RichStyleTable {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            free: Vec::new(),
            index: vec![0; STD_INDEX_SLOTS].into_boxed_slice(),
            live: 0,
            hashes: 0,
        }
    }

    /// Live ids.
    pub fn live(&self) -> usize {
        self.live as usize
    }

    pub fn index_slots(&self) -> usize {
        self.index.len()
    }

    /// Hashed lookups since the last reset.
    pub fn hash_count(&self) -> u64 {
        self.hashes
    }

    /// One past the largest id ever handed out since the last reset.
    pub fn id_bound(&self) -> usize {
        self.entries.len() + 1
    }

    fn entry(&self, id: u32) -> Option<&RichEntry> {
        let pos = (id as usize).checked_sub(1)?;
        self.entries.get(pos).filter(|entry| entry.refs > 0)
    }

    /// The style of a live id.
    pub fn get(&self, id: u32) -> Option<&RichStyle> {
        self.entry(id).map(|entry| &entry.style)
    }

    /// References held on `id`; 0 when it is free or was never handed out.
    pub fn refs(&self, id: u32) -> u32 {
        self.entry(id).map_or(0, |entry| entry.refs)
    }

    /// Takes a reference on the id for `style`, adding an entry if needed.
    /// A valid `hint` makes a repeat O(1) with no hashing.
    pub fn acquire(
        &mut self,
        style: &RichStyle,
        page_serial: u64,
        hint: Option<CachedRichId>,
    ) -> (u32, CachedRichId) {
        if let Some(hint) = hint {
            if hint.page_serial == page_serial {
                if let Some(entry) = (hint.id as usize)
                    .checked_sub(1)
                    .and_then(|pos| self.entries.get_mut(pos))
                {
                    if entry.refs > 0
                        && entry.generation == hint.generation
                        && entry.style == *style
                    {
                        entry.refs += 1;
                        return (hint.id, hint);
                    }
                }
            }
        }

        self.hashes += 1;
        let hash = style.hash32();
        let mask = self.index.len() - 1;
        let mut slot = hash as usize & mask;
        loop {
            let id = self.index[slot];
            if id == 0 {
                break;
            }
            let entry = &mut self.entries[id as usize - 1];
            if entry.hash == hash && entry.style == *style {
                entry.refs += 1;
                let cached = CachedRichId {
                    page_serial,
                    id,
                    generation: entry.generation,
                };
                return (id, cached);
            }
            slot = (slot + 1) & mask;
        }

        if (self.live as usize + 1) * 4 > self.index.len() * 3 {
            self.grow_index();
            slot = self.empty_slot_for(hash);
        }
        let id = match self.free.pop() {
            Some(id) => {
                let entry = &mut self.entries[id as usize - 1];
                entry.style = *style;
                entry.hash = hash;
                entry.refs = 1;
                id
            }
            None => {
                self.entries.push(RichEntry {
                    style: *style,
                    hash,
                    refs: 1,
                    generation: 0,
                });
                debug_assert!(self.entries.len() <= u32::MAX as usize);
                self.entries.len() as u32
            }
        };
        self.index[slot] = id;
        self.live += 1;
        let cached = CachedRichId {
            page_serial,
            id,
            generation: self.entries[id as usize - 1].generation,
        };
        (id, cached)
    }

    /// Takes another reference on a live id (a wide glyph's spacer, a cell
    /// copied within the page).
    pub fn add_ref(&mut self, id: u32) {
        let entry = &mut self.entries[id as usize - 1];
        debug_assert!(entry.refs > 0, "add_ref on free rich id {}", id);
        entry.refs += 1;
    }

    /// Drops a reference; the last one frees the id and bumps its
    /// generation, which invalidates every cached hint for it.
    pub fn release(&mut self, id: u32) {
        let entry = &mut self.entries[id as usize - 1];
        debug_assert!(entry.refs > 0, "release of free rich id {}", id);
        entry.refs -= 1;
        if entry.refs == 0 {
            entry.generation = entry.generation.wrapping_add(1);
            let hash = entry.hash;
            self.remove_from_index(id, hash);
            self.free.push(id);
            self.live -= 1;
        }
    }

    fn empty_slot_for(&self, hash: u32) -> usize {
        let mask = self.index.len() - 1;
        let mut slot = hash as usize & mask;
        while self.index[slot] != 0 {
            slot = (slot + 1) & mask;
        }
        slot
    }

    /// Doubles the index and rehashes the live ids into it. Entries do not
    /// move, so cells keep their ids.
    fn grow_index(&mut self) {
        let slots = self.index.len() * 2;
        self.index = vec![0; slots].into_boxed_slice();
        for (pos, entry) in self.entries.iter().enumerate() {
            if entry.refs > 0 {
                let slot = self.empty_slot_for(entry.hash);
                self.index[slot] = (pos + 1) as u32;
            }
        }
    }

    /// Linear-probing delete by backward shift: later members of the probe
    /// run move into the hole whenever their home slot does not lie
    /// cyclically in `(hole, slot]`.
    fn remove_from_index(&mut self, id: u32, hash: u32) {
        let mask = self.index.len() - 1;
        let mut hole = hash as usize & mask;
        while self.index[hole] != id {
            debug_assert_ne!(self.index[hole], 0, "rich id {} missing from index", id);
            hole = (hole + 1) & mask;
        }
        let mut slot = hole;
        loop {
            slot = (slot + 1) & mask;
            let other = self.index[slot];
            if other == 0 {
                break;
            }
            let home = self.entries[other as usize - 1].hash as usize & mask;
            let stays = if hole <= slot {
                hole < home && home <= slot
            } else {
                hole < home || home <= slot
            };
            if !stays {
                self.index[hole] = other;
                hole = slot;
            }
        }
        self.index[hole] = 0;
    }

    /// Back to a new table's state: no entries, a 512-slot index.
    pub fn reset(&mut self) {
        self.entries = Vec::new();
        self.free = Vec::new();
        if self.index.len() == STD_INDEX_SLOTS {
            self.index.fill(0);
        } else {
            self.index = vec![0; STD_INDEX_SLOTS].into_boxed_slice();
        }
        self.live = 0;
        self.hashes = 0;
    }

    /// Whether the table is in a new table's state (I14).
    pub fn is_clean(&self) -> bool {
        self.entries.capacity() == 0
            && self.free.capacity() == 0
            && self.index.len() == STD_INDEX_SLOTS
            && self.index.iter().all(|&slot| slot == 0)
            && self.live == 0
    }

    /// I5: `cell_refs[id]` is the number of stored cells holding `id`.
    pub(crate) fn check(&self, cell_refs: &[u32]) -> Result<(), String> {
        if cell_refs.len() > self.id_bound() && cell_refs[self.id_bound()..].iter().any(|&n| n > 0)
        {
            return Err("a cell holds a rich id that was never handed out".to_string());
        }
        let mut live = 0;
        for (pos, entry) in self.entries.iter().enumerate() {
            let id = pos + 1;
            let cells = cell_refs.get(id).copied().unwrap_or(0);
            if entry.refs != cells {
                return Err(format!(
                    "rich id {} has refs {} but {} cells",
                    id, entry.refs, cells
                ));
            }
            if entry.refs > 0 {
                live += 1;
                if entry.hash != entry.style.hash32() {
                    return Err(format!("rich id {} has a stale hash", id));
                }
            }
        }
        if live != self.live {
            return Err(format!(
                "live count {} but {} live entries",
                self.live, live
            ));
        }
        let mut free = self.free.clone();
        free.sort_unstable();
        free.dedup();
        if free.len() != self.free.len() {
            return Err("free list holds an id twice".to_string());
        }
        if free.len() + live as usize != self.entries.len()
            || free.iter().any(|&id| self.entry(id).is_some())
        {
            return Err("free list does not match the free entries".to_string());
        }
        if (self.live as usize) * 4 > self.index.len() * 3 || !self.index.len().is_power_of_two() {
            return Err(format!(
                "index of {} slots holds {} ids",
                self.index.len(),
                self.live
            ));
        }
        let mut indexed = 0;
        let mut seen = vec![false; self.id_bound()];
        let mask = self.index.len() - 1;
        for (slot, &id) in self.index.iter().enumerate() {
            if id == 0 {
                continue;
            }
            indexed += 1;
            let entry = self
                .entry(id)
                .ok_or_else(|| format!("index slot {} holds free id {}", slot, id))?;
            if std::mem::replace(&mut seen[id as usize], true) {
                return Err(format!("rich id {} is indexed twice", id));
            }
            // Every slot from the home slot up to this one is occupied, so a
            // probe for this entry reaches it.
            let mut probe = entry.hash as usize & mask;
            while probe != slot {
                if self.index[probe] == 0 {
                    return Err(format!("rich id {} is unreachable from its home slot", id));
                }
                probe = (probe + 1) & mask;
            }
        }
        if indexed != live {
            return Err(format!(
                "index holds {} ids for {} live entries",
                indexed, live
            ));
        }
        Ok(())
    }
}
