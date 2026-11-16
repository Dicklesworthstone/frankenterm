//! Bounded, shared source bitmaps for pane-local glyph atlases.
//!
//! A font configuration is shared by the panes in a window, but their atlas
//! caches are independent. Keep the expensive native raster result here so
//! opening another pane or rebuilding an atlas does not rasterize the same
//! face/glyph/size again. Layout and atlas residency remain with the caller.

use super::RasterizedGlyph;
use crate::{FallbackIdx, LoadedFontId};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

const MAX_BITMAP_BYTES: usize = 4 * 1024 * 1024;
const MAX_ENTRIES: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct RasterKey {
    /// Includes font size, DPI, variation/synthesis settings and configured
    /// face ordering. Dynamic fallback faces only append to this font.
    pub font: LoadedFontId,
    pub fallback: FallbackIdx,
    pub glyph: u32,
}

struct Entry {
    glyph: Rc<RasterizedGlyph>,
    referenced: bool,
}

/// Second-chance eviction needs one queue entry per bitmap and no allocation
/// or queue growth on hits. Both retained pixel capacity and entry count are
/// bounded across *all* loaded fonts/styles in the configuration, not per pane.
pub(crate) struct RasterCache {
    entries: HashMap<RasterKey, Entry>,
    order: VecDeque<RasterKey>,
    bytes: usize,
    max_bytes: usize,
    max_entries: usize,
}

impl Default for RasterCache {
    fn default() -> Self {
        Self::with_limits(MAX_BITMAP_BYTES, MAX_ENTRIES)
    }
}

impl RasterCache {
    fn with_limits(max_bytes: usize, max_entries: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            max_bytes,
            max_entries,
        }
    }

    pub fn get(&mut self, key: RasterKey) -> Option<Rc<RasterizedGlyph>> {
        let entry = self.entries.get_mut(&key)?;
        entry.referenced = true;
        Some(Rc::clone(&entry.glyph))
    }

    pub fn insert(&mut self, key: RasterKey, glyph: Rc<RasterizedGlyph>) {
        // Count allocation capacity, including spare storage owned by the
        // rasterizer, rather than just the bitmap's visible pixel length.
        let bytes = glyph.data.capacity();
        if bytes > self.max_bytes || self.max_entries == 0 || self.entries.contains_key(&key) {
            return;
        }
        while self.entries.len() >= self.max_entries || self.bytes > self.max_bytes - bytes {
            let oldest = self.order.pop_front().expect("nonempty raster cache");
            let entry = self.entries.get_mut(&oldest).expect("queued raster entry");
            if entry.referenced {
                entry.referenced = false;
                self.order.push_back(oldest);
            } else {
                self.bytes -= entry.glyph.data.capacity();
                self.entries.remove(&oldest);
            }
        }
        self.bytes += bytes;
        self.entries.insert(
            key,
            Entry {
                glyph,
                referenced: false,
            },
        );
        self.order.push_back(key);
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::units::PixelLength;
    use euclid::num::Zero;

    fn key(font: LoadedFontId, fallback: FallbackIdx, glyph: u32) -> RasterKey {
        RasterKey {
            font,
            fallback,
            glyph,
        }
    }

    fn bitmap(pixel: u8, capacity: usize) -> Rc<RasterizedGlyph> {
        let mut data = Vec::with_capacity(capacity);
        data.extend_from_slice(&[pixel; 4]);
        Rc::new(RasterizedGlyph {
            data,
            height: 1,
            width: 1,
            bearing_x: PixelLength::zero(),
            bearing_y: PixelLength::zero(),
            has_color: true,
            is_scaled: true,
        })
    }

    #[test]
    fn consumers_share_pixels_and_full_font_face_glyph_identity() {
        let mut cache = RasterCache::with_limits(32, 8);
        let keys = [key(1, 0, 1), key(2, 0, 1), key(1, 1, 1), key(1, 0, 2)];
        for (index, key) in keys.iter().enumerate() {
            cache.insert(*key, bitmap(index as u8, 4));
        }
        let first = cache.get(keys[0]).unwrap();
        for _ in 0..200 {
            assert!(Rc::ptr_eq(&first, &cache.get(keys[0]).unwrap()));
        }
        for (index, key) in keys.iter().enumerate() {
            assert_eq!(cache.get(*key).unwrap().data, vec![index as u8; 4]);
        }
        assert_eq!(cache.order.len(), keys.len(), "hits cannot grow metadata");
        assert_eq!(cache.bytes, 16);
    }

    #[test]
    fn hot_bitmap_gets_a_second_chance_and_eviction_keeps_borrowers_valid() {
        let mut cache = RasterCache::with_limits(8, 8);
        let a = key(1, 0, 1);
        let b = key(1, 0, 2);
        let c = key(2, 0, 3);
        let retained_b = bitmap(2, 4);
        cache.insert(a, bitmap(1, 4));
        cache.insert(b, Rc::clone(&retained_b));
        cache.get(a).unwrap();
        cache.insert(c, bitmap(3, 4));
        assert!(cache.get(a).is_some());
        assert!(cache.get(b).is_none());
        assert_eq!(retained_b.data, vec![2; 4]);
        assert_eq!(cache.get(c).unwrap().data, vec![3; 4]);
        assert_eq!(cache.bytes, 8);
        assert_eq!(cache.order.len(), 2);
    }

    #[test]
    fn oversized_allocation_bypasses_without_evicting_working_set() {
        let mut cache = RasterCache::with_limits(8, 8);
        let small = key(1, 0, 1);
        let large = key(1, 0, 2);
        cache.insert(small, bitmap(1, 4));
        // The visible bitmap is only four bytes; retained spare capacity is
        // still subject to the cap.
        cache.insert(large, bitmap(2, 1024));
        assert!(cache.get(small).is_some());
        assert!(cache.get(large).is_none());
        assert_eq!(cache.bytes, 4);
        assert_eq!(cache.entries.len(), 1);
    }

    #[test]
    fn inkless_glyphs_and_duplicate_inserts_cannot_escape_entry_bound() {
        let mut cache = RasterCache::with_limits(0, 2);
        for glyph in 0..64 {
            let empty = Rc::new(RasterizedGlyph {
                data: Vec::new(),
                height: 0,
                width: 0,
                bearing_x: PixelLength::zero(),
                bearing_y: PixelLength::zero(),
                has_color: false,
                is_scaled: true,
            });
            let key = key(1, 0, glyph);
            cache.insert(key, Rc::clone(&empty));
            cache.insert(key, empty);
            assert!(cache.entries.len() <= 2);
            assert_eq!(cache.order.len(), cache.entries.len());
            assert_eq!(cache.bytes, 0);
        }
        cache.clear();
        assert!(cache.entries.is_empty());
        assert!(cache.order.is_empty());
        assert_eq!(cache.bytes, 0);
    }
}
