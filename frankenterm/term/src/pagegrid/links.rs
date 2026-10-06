//! Per-page hyperlink table and image map (PageGrid ADR sections 3.6, 3.7).

// The image map keeps legacy's `Vec<Box<ImageCell>>` per cell unchanged
// (ADR section 3.7).
#![allow(clippy::vec_box)]

use super::hash::FxHashMap;
use frankenterm_cell::image::ImageCell;
use frankenterm_cell::Hyperlink;
use std::sync::Arc;

/// Link entries a reset page keeps room for at most.
pub const STD_LINKS: usize = 4;

#[derive(Clone, Debug)]
struct LinkEntry {
    link: Arc<Hyperlink>,
    refs: u32,
}

/// Explicit (OSC 8) links. The table keeps the pen's own `Arc` and
/// deduplicates by pointer, not by value: legacy and the wire codec split
/// hyperlink spans by pointer identity.
#[derive(Clone, Debug, Default)]
pub struct LinkTable {
    /// Entry for id `k` is `entries[k - 1]`.
    entries: Vec<Option<LinkEntry>>,
    free: Vec<u32>,
    /// `Arc` address to id. An address cannot be reused while the table
    /// holds a strong reference to it.
    by_ptr: FxHashMap<usize, u32>,
    /// Cell offset to id.
    cells: FxHashMap<u32, u32>,
}

fn ptr_key(link: &Arc<Hyperlink>) -> usize {
    Arc::as_ptr(link) as usize
}

impl LinkTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Live link entries.
    pub fn live(&self) -> usize {
        self.by_ptr.len()
    }

    /// Cells holding a link.
    pub fn cell_count(&self) -> usize {
        self.cells.len()
    }

    pub fn contains(&self, key: u32) -> bool {
        self.cells.contains_key(&key)
    }

    pub fn get(&self, key: u32) -> Option<&Arc<Hyperlink>> {
        let id = *self.cells.get(&key)?;
        self.entries[id as usize - 1]
            .as_ref()
            .map(|entry| &entry.link)
    }

    /// Links the cell at `key`, which must not hold a link.
    pub fn attach(&mut self, key: u32, link: &Arc<Hyperlink>) {
        let id = match self.by_ptr.get(&ptr_key(link)) {
            Some(&id) => {
                self.entry_mut(id).refs += 1;
                id
            }
            None => {
                let entry = LinkEntry {
                    link: Arc::clone(link),
                    refs: 1,
                };
                let id = match self.free.pop() {
                    Some(id) => {
                        self.entries[id as usize - 1] = Some(entry);
                        id
                    }
                    None => {
                        self.entries.push(Some(entry));
                        self.entries.len() as u32
                    }
                };
                self.by_ptr.insert(ptr_key(link), id);
                id
            }
        };
        let previous = self.cells.insert(key, id);
        debug_assert!(previous.is_none(), "cell {} already has a link", key);
    }

    /// Links `to` to the same entry as `from`.
    pub fn duplicate(&mut self, from: u32, to: u32) {
        let Some(&id) = self.cells.get(&from) else {
            debug_assert!(false, "cell {} has no link to copy", from);
            return;
        };
        self.entry_mut(id).refs += 1;
        let previous = self.cells.insert(to, id);
        debug_assert!(previous.is_none(), "cell {} already has a link", to);
    }

    /// Unlinks the cell; false when it has no link.
    pub fn detach(&mut self, key: u32) -> bool {
        let Some(id) = self.cells.remove(&key) else {
            return false;
        };
        let entry = self.entry_mut(id);
        entry.refs -= 1;
        if entry.refs == 0 {
            if let Some(entry) = self.entries[id as usize - 1].take() {
                self.by_ptr.remove(&ptr_key(&entry.link));
            }
            self.free.push(id);
        }
        true
    }

    fn entry_mut(&mut self, id: u32) -> &mut LinkEntry {
        self.entries[id as usize - 1]
            .as_mut()
            .expect("a mapped link id is live (I7)")
    }

    /// Detaches the cell's id without releasing it, for re-keying.
    pub(crate) fn take(&mut self, key: u32) -> Option<u32> {
        self.cells.remove(&key)
    }

    pub(crate) fn put(&mut self, key: u32, id: u32) {
        let previous = self.cells.insert(key, id);
        debug_assert!(previous.is_none(), "cell {} already has a link", key);
    }

    pub fn reset(&mut self) {
        self.entries.clear();
        self.entries.shrink_to(STD_LINKS);
        self.free.clear();
        self.free.shrink_to(STD_LINKS);
        self.by_ptr.clear();
        self.by_ptr.shrink_to(STD_LINKS);
        self.cells.clear();
        self.cells.shrink_to(0);
    }

    /// Empty and within standard capacity (I14).
    pub fn is_clean(&self) -> bool {
        // Computed once: `Page::reset` checks this in debug builds, and a
        // recycled page must not allocate.
        static STD_PTR_CAPACITY: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
        let std_ptr_capacity = *STD_PTR_CAPACITY.get_or_init(|| {
            FxHashMap::<usize, u32>::with_capacity_and_hasher(STD_LINKS, Default::default())
                .capacity()
        });
        self.entries.is_empty()
            && self.entries.capacity() <= STD_LINKS
            && self.free.is_empty()
            && self.free.capacity() <= STD_LINKS
            && self.by_ptr.is_empty()
            && self.by_ptr.capacity() <= std_ptr_capacity
            && self.cells.is_empty()
            && self.cells.capacity() == 0
    }

    pub(crate) fn keys(&self) -> impl Iterator<Item = u32> + '_ {
        self.cells.keys().copied()
    }

    /// I7, table half: every mapped id is live, each entry's refs equal the
    /// cells naming it, and the pointer index and free list agree.
    pub(crate) fn check(&self) -> Result<(), String> {
        let mut counts = vec![0_u32; self.entries.len() + 1];
        for (&key, &id) in &self.cells {
            match self.entries.get((id as usize).wrapping_sub(1)) {
                Some(Some(_)) => counts[id as usize] += 1,
                _ => return Err(format!("cell {} names dead link id {}", key, id)),
            }
        }
        let mut live = 0;
        for (pos, entry) in self.entries.iter().enumerate() {
            let id = pos as u32 + 1;
            match entry {
                Some(entry) => {
                    live += 1;
                    if entry.refs != counts[id as usize] {
                        return Err(format!(
                            "link id {} has refs {} but {} cells",
                            id, entry.refs, counts[id as usize]
                        ));
                    }
                    if self.by_ptr.get(&ptr_key(&entry.link)) != Some(&id) {
                        return Err(format!("link id {} is missing from the pointer index", id));
                    }
                }
                None => {
                    if !self.free.contains(&id) {
                        return Err(format!("dead link id {} is not on the free list", id));
                    }
                }
            }
        }
        if live != self.by_ptr.len() || live + self.free.len() != self.entries.len() {
            return Err("link pointer index or free list is out of step".to_string());
        }
        Ok(())
    }
}

/// Images per cell, kept as legacy's z-ordered `Vec<Box<ImageCell>>`.
#[derive(Clone, Debug, Default)]
pub struct ImageMap {
    cells: FxHashMap<u32, Vec<Box<ImageCell>>>,
}

impl ImageMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cell_count(&self) -> usize {
        self.cells.len()
    }

    pub fn contains(&self, key: u32) -> bool {
        self.cells.contains_key(&key)
    }

    pub fn get(&self, key: u32) -> Option<&[Box<ImageCell>]> {
        self.cells.get(&key).map(Vec::as_slice)
    }

    /// Stores a non-empty image list for a cell that has none.
    pub fn insert(&mut self, key: u32, images: Vec<Box<ImageCell>>) {
        debug_assert!(!images.is_empty());
        let previous = self.cells.insert(key, images);
        debug_assert!(previous.is_none(), "cell {} already has images", key);
    }

    pub fn remove(&mut self, key: u32) -> Option<Vec<Box<ImageCell>>> {
        self.cells.remove(&key)
    }

    pub fn reset(&mut self) {
        self.cells.clear();
        self.cells.shrink_to(0);
    }

    pub fn is_clean(&self) -> bool {
        self.cells.is_empty() && self.cells.capacity() == 0
    }

    pub(crate) fn keys(&self) -> impl Iterator<Item = u32> + '_ {
        self.cells.keys().copied()
    }
}

/// Legacy `CellAttributes::attach_image`: insert keeping z-index order.
pub fn attach_image(images: &mut Vec<Box<ImageCell>>, image: Box<ImageCell>) {
    let z_index = image.z_index();
    match images.binary_search_by(|probe| probe.z_index().cmp(&z_index)) {
        Ok(idx) | Err(idx) => images.insert(idx, image),
    }
}
