/// Per-page slot bitmap for record locks.
///
/// InnoDB uses a bitmap in each `lock_rec_t` to track which heap records
/// (slot_ids) on a page are covered by the lock. This avoids creating one
/// lock struct per row — a single `LockEntry` with a `SlotBitmap` covers
/// all locked slots on the same page with the same mode.
///
/// Backed by `Vec<u64>` that grows on demand. Typical pages have ≤256 slots,
/// so 4 words (32 bytes) suffice. Never shrinks — reused across operations.
#[derive(Debug, Clone, Default)]
pub struct SlotBitmap {
    words: Vec<u64>,
}

impl SlotBitmap {
    /// Creates an empty bitmap.
    pub fn new() -> Self {
        Self { words: Vec::new() }
    }

    /// Creates a bitmap with a single bit set.
    pub fn with_slot(slot_id: u16) -> Self {
        let mut bm = Self::new();
        bm.set(slot_id);
        bm
    }

    /// Sets the bit for `slot_id`.
    pub fn set(&mut self, slot_id: u16) {
        let word_idx = (slot_id / 64) as usize;
        let bit_idx = slot_id % 64;
        if word_idx >= self.words.len() {
            self.words.resize(word_idx + 1, 0);
        }
        self.words[word_idx] |= 1u64 << bit_idx;
    }

    /// Clears the bit for `slot_id`.
    pub fn clear(&mut self, slot_id: u16) {
        let word_idx = (slot_id / 64) as usize;
        let bit_idx = slot_id % 64;
        if word_idx < self.words.len() {
            self.words[word_idx] &= !(1u64 << bit_idx);
        }
    }

    /// Returns `true` if the bit for `slot_id` is set.
    pub fn test(&self, slot_id: u16) -> bool {
        let word_idx = (slot_id / 64) as usize;
        let bit_idx = slot_id % 64;
        if word_idx >= self.words.len() {
            return false;
        }
        (self.words[word_idx] & (1u64 << bit_idx)) != 0
    }

    /// Returns `true` if any bit is set in both bitmaps (overlap check).
    pub fn overlaps(&self, other: &SlotBitmap) -> bool {
        let min_len = self.words.len().min(other.words.len());
        for i in 0..min_len {
            if (self.words[i] & other.words[i]) != 0 {
                return true;
            }
        }
        false
    }

    /// Returns `true` if a specific slot overlaps with this bitmap.
    #[inline]
    pub fn overlaps_slot(&self, slot_id: u16) -> bool {
        self.test(slot_id)
    }

    /// Returns `true` if no bits are set.
    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|&w| w == 0)
    }

    /// Clears all bits.
    pub fn clear_all(&mut self) {
        for w in &mut self.words {
            *w = 0;
        }
    }

    /// Returns the number of set bits.
    pub fn count(&self) -> u32 {
        self.words.iter().map(|w| w.count_ones()).sum()
    }

    /// Returns the first set bit, or `None` if empty.
    pub fn first_set_bit(&self) -> Option<u16> {
        for (word_idx, &w) in self.words.iter().enumerate() {
            if w != 0 {
                return Some((word_idx as u16) * 64 + w.trailing_zeros() as u16);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_clear_test() {
        let mut bm = SlotBitmap::new();
        assert!(!bm.test(5));
        bm.set(5);
        assert!(bm.test(5));
        assert!(!bm.test(4));
        bm.clear(5);
        assert!(!bm.test(5));
    }

    #[test]
    fn grows_on_demand() {
        let mut bm = SlotBitmap::new();
        bm.set(200); // word index 3
        assert!(bm.test(200));
        assert!(!bm.test(199));
        assert_eq!(bm.words.len(), 4);
    }

    #[test]
    fn overlaps_disjoint() {
        let mut a = SlotBitmap::new();
        let mut b = SlotBitmap::new();
        a.set(1);
        a.set(3);
        b.set(2);
        b.set(4);
        assert!(!a.overlaps(&b));
    }

    #[test]
    fn overlaps_shared() {
        let mut a = SlotBitmap::new();
        let mut b = SlotBitmap::new();
        a.set(5);
        a.set(10);
        b.set(10);
        b.set(20);
        assert!(a.overlaps(&b));
    }

    #[test]
    fn is_empty_and_count() {
        let mut bm = SlotBitmap::new();
        assert!(bm.is_empty());
        bm.set(0);
        bm.set(63);
        bm.set(64);
        assert!(!bm.is_empty());
        assert_eq!(bm.count(), 3);
        bm.clear(0);
        bm.clear(63);
        bm.clear(64);
        assert!(bm.is_empty());
    }

    #[test]
    fn with_slot_constructor() {
        let bm = SlotBitmap::with_slot(42);
        assert!(bm.test(42));
        assert_eq!(bm.count(), 1);
    }
}
