use axiomdb_core::error::DbError;

use crate::page::{HEADER_SIZE, PAGE_SIZE};

// ── Constants ─────────────────────────────────────────────────────────────────

/// Available bytes in the body of the bitmap page.
const BITMAP_BODY_BYTES: usize = PAGE_SIZE - HEADER_SIZE;
/// Maximum pages covered by a single bitmap (130,560).
pub const BITMAP_CAPACITY: u64 = (BITMAP_BODY_BYTES * 8) as u64;

// ── FreeList ──────────────────────────────────────────────────────────────────

/// In-memory free page bitmap.
///
/// Convention: bit = 1 → FREE, bit = 0 → USED.
/// Bits are organized in 64-bit words, LSB-first:
///   word[0] covers pages 0-63, word[1] covers 64-127, etc.
///
/// `alloc` uses `u64::trailing_zeros()` on the inverted word to find
/// the first free bit in O(1) per word, O(n/64) total.
#[derive(Debug)]
pub struct FreeList {
    words: Vec<u64>,
    total_pages: u64,
}

impl FreeList {
    /// Creates a new bitmap for `total_pages` pages.
    ///
    /// Pages in `reserved` are marked as USED.
    /// All others (within range) are marked as FREE.
    pub fn new(total_pages: u64, reserved: &[u64]) -> Self {
        assert!(
            total_pages <= BITMAP_CAPACITY,
            "total_pages {total_pages} exceeds BITMAP_CAPACITY {BITMAP_CAPACITY}"
        );

        let n_words = Self::words_needed(total_pages);
        // Initialize all to FREE (0xFF...).
        let mut words = vec![u64::MAX; n_words];

        // Mark bits beyond total_pages as USED (they don't exist).
        Self::mask_tail(&mut words, total_pages);

        // Mark reserved pages as USED.
        let mut fl = FreeList { words, total_pages };
        for &page_id in reserved {
            fl.mark_used(page_id);
        }
        fl
    }

    /// Deserializes a FreeList from the body of the bitmap page.
    pub fn from_bytes(bytes: &[u8], total_pages: u64) -> Self {
        assert!(bytes.len() >= BITMAP_BODY_BYTES);
        assert!(total_pages <= BITMAP_CAPACITY);

        let n_words = Self::words_needed(total_pages);
        let mut words = vec![0u64; n_words];
        for (i, w) in words.iter_mut().enumerate() {
            let off = i * 8;
            // Direct array construction: off + 8 <= BITMAP_BODY_BYTES (asserted above)
            // and i < n_words, so the indexing is in bounds.
            *w = u64::from_le_bytes([
                bytes[off],
                bytes[off + 1],
                bytes[off + 2],
                bytes[off + 3],
                bytes[off + 4],
                bytes[off + 5],
                bytes[off + 6],
                bytes[off + 7],
            ]);
        }
        // Ensure leftover bits are zero (USED).
        Self::mask_tail(&mut words, total_pages);
        FreeList { words, total_pages }
    }

    /// Serializes the bitmap to buffer `buf` (must be ≥ BITMAP_BODY_BYTES).
    pub fn to_bytes(&self, buf: &mut [u8]) {
        assert!(buf.len() >= BITMAP_BODY_BYTES);
        // Clear first (in case there were stale data beyond the active bitmap).
        buf[..BITMAP_BODY_BYTES].fill(0);
        for (i, &w) in self.words.iter().enumerate() {
            let off = i * 8;
            buf[off..off + 8].copy_from_slice(&w.to_le_bytes());
        }
    }

    /// Finds and reserves the first free page.
    ///
    /// Returns `None` if the bitmap is full (time to grow).
    /// Complexity: O(n/64) where n = total_pages.
    pub fn alloc(&mut self) -> Option<u64> {
        for (i, word) in self.words.iter_mut().enumerate() {
            if *word == 0 {
                continue;
            }
            // trailing_zeros on word gives the index of the lowest free bit.
            let bit = word.trailing_zeros() as u64;
            let page_id = i as u64 * 64 + bit;
            if page_id < self.total_pages {
                // Mark as USED.
                *word &= !(1u64 << bit);
                return Some(page_id);
            }
        }
        None
    }

    /// Marks `page_id` as free.
    ///
    /// Returns an error if `page_id` is out of range or already free (double-free).
    pub fn free(&mut self, page_id: u64) -> Result<(), DbError> {
        if page_id >= self.total_pages {
            return Err(DbError::PageNotFound { page_id });
        }
        let (word_idx, bit) = Self::bit_pos(page_id);
        let mask = 1u64 << bit;
        if self.words[word_idx] & mask != 0 {
            return Err(DbError::Other(format!(
                "double-free detected on page {page_id}"
            )));
        }
        self.words[word_idx] |= mask;
        Ok(())
    }

    /// Marks `page_id` as USED without checking if it already was.
    pub fn mark_used(&mut self, page_id: u64) {
        if page_id >= self.total_pages {
            return;
        }
        let (word_idx, bit) = Self::bit_pos(page_id);
        self.words[word_idx] &= !(1u64 << bit);
    }

    /// Extends the bitmap to cover `new_total` pages.
    ///
    /// New pages (old_total..new_total) are marked as FREE.
    pub fn grow(&mut self, new_total: u64) {
        assert!(new_total > self.total_pages);
        assert!(
            new_total <= BITMAP_CAPACITY,
            "new_total {new_total} exceeds BITMAP_CAPACITY"
        );

        let old_total = self.total_pages;
        let new_n_words = Self::words_needed(new_total);

        // Extend the vector with words filled with FREE.
        self.words.resize(new_n_words, u64::MAX);
        self.total_pages = new_total;

        // Ensure bits in the last old word that were previously marked
        // as "out of range" (USED) are now marked FREE.
        let old_n_words = Self::words_needed(old_total);
        if old_n_words > 0 {
            let last_old_idx = old_n_words - 1;
            let bits_in_last = old_total % 64;
            if bits_in_last != 0 {
                // The word had upper bits forced to USED; they are now FREE.
                let free_mask = u64::MAX << bits_in_last;
                self.words[last_old_idx] |= free_mask;
            }
        }

        // Re-mask bits beyond new_total.
        Self::mask_tail(&mut self.words, new_total);
    }

    /// Total number of pages covered by this bitmap.
    pub fn total_pages(&self) -> u64 {
        self.total_pages
    }

    /// Returns `true` if `page_id` is free (not yet allocated).
    ///
    /// Out-of-range page IDs are treated as not-free (they don't exist).
    pub fn is_free(&self, page_id: u64) -> bool {
        if page_id >= self.total_pages {
            return false;
        }
        let (word_idx, bit) = Self::bit_pos(page_id);
        (self.words[word_idx] >> bit) & 1 == 1
    }

    /// Number of currently free pages.
    pub fn free_count(&self) -> u64 {
        self.words.iter().map(|w| w.count_ones() as u64).sum()
    }

    // ── Batch operations (Phase 40.9) ────────────────────────────────────────

    /// Allocates up to `n` pages, preferring a contiguous run for
    /// sequential locality (OS prefetch friendly).
    ///
    /// Returns **up to** `n` page IDs. If the bitmap has fewer than `n`
    /// free pages, the returned Vec is shorter — the caller must check the
    /// length and grow the file if needed.
    ///
    /// Strategy:
    /// 1. Scan words for a contiguous run of `n` free bits.
    /// 2. If found, mark them all USED and return.
    /// 3. If no contiguous run, fall back to `alloc()` in a loop
    ///    (non-contiguous fill).
    pub fn alloc_batch_sequential(&mut self, n: usize) -> Vec<u64> {
        if n == 0 {
            return Vec::new();
        }

        // Strategy 1: try to find a contiguous run.
        if let Some(start) = self.find_contiguous_run(n) {
            for offset in 0..(n as u64) {
                self.mark_used(start + offset);
            }
            return (start..start + n as u64).collect();
        }

        // Strategy 2: non-contiguous fill via individual alloc().
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            match self.alloc() {
                Some(id) => out.push(id),
                None => break,
            }
        }
        out
    }

    /// Marks all `ids` as free in one pass. Returns the first error
    /// encountered (if any), but continues marking the rest.
    pub fn free_batch(&mut self, ids: &[u64]) -> Result<(), DbError> {
        let mut first_err = None;
        for &page_id in ids {
            if let Err(e) = self.free(page_id) {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// Scans the bitmap for a contiguous run of `n` free bits.
    ///
    /// Returns the page ID of the first bit, or `None` if no run of length
    /// `n` exists. O(total_pages / 64) worst case.
    fn find_contiguous_run(&self, n: usize) -> Option<u64> {
        if n == 0 {
            return Some(0);
        }
        let n = n as u64;
        let mut run_start = 0u64;
        let mut run_len = 0u64;

        for (i, &word) in self.words.iter().enumerate() {
            let base = i as u64 * 64;
            if word == u64::MAX {
                // All 64 bits free — extend or start a run.
                if run_len == 0 {
                    run_start = base;
                }
                run_len += 64;
                // Clamp to total_pages.
                let run_end = (run_start + run_len).min(self.total_pages);
                if run_end - run_start >= n {
                    return Some(run_start);
                }
                continue;
            }
            if word == 0 {
                // All 64 bits used — break any run.
                run_len = 0;
                continue;
            }
            // Partially free word — check bit by bit for run boundaries.
            for bit in 0..64u64 {
                let page_id = base + bit;
                if page_id >= self.total_pages {
                    break;
                }
                if (word >> bit) & 1 == 1 {
                    // Free bit.
                    if run_len == 0 {
                        run_start = page_id;
                    }
                    run_len += 1;
                    if run_len >= n {
                        return Some(run_start);
                    }
                } else {
                    // Used bit — break the run.
                    run_len = 0;
                }
            }
        }
        None
    }

    #[inline]
    fn words_needed(n_pages: u64) -> usize {
        n_pages.div_ceil(64) as usize
    }

    #[inline]
    fn bit_pos(page_id: u64) -> (usize, u32) {
        ((page_id / 64) as usize, (page_id % 64) as u32)
    }

    /// Forces all bits in the last word beyond `total` to USED.
    fn mask_tail(words: &mut [u64], total: u64) {
        let remainder = total % 64;
        if remainder != 0 && !words.is_empty() {
            let last = words.len() - 1;
            // Bits [remainder..63] must be 0 (USED / do not exist).
            let valid_mask = (1u64 << remainder) - 1;
            words[last] &= valid_mask;
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_fl(total: u64) -> FreeList {
        // Reserve pages 0 and 1 as in real storage.
        FreeList::new(total, &[0, 1])
    }

    #[test]
    fn test_alloc_starts_from_2() {
        let mut fl = make_fl(64);
        assert_eq!(fl.alloc(), Some(2));
        assert_eq!(fl.alloc(), Some(3));
        assert_eq!(fl.alloc(), Some(4));
    }

    #[test]
    fn test_alloc_consecutive_all_pages() {
        let mut fl = make_fl(64);
        let mut ids: Vec<u64> = (0..62).map(|_| fl.alloc().unwrap()).collect();
        ids.sort();
        assert_eq!(ids, (2u64..64).collect::<Vec<_>>());
        // Now it is full.
        assert_eq!(fl.alloc(), None);
    }

    #[test]
    fn test_free_and_realloc() {
        let mut fl = make_fl(64);
        let id1 = fl.alloc().unwrap(); // 2
        let id2 = fl.alloc().unwrap(); // 3
        fl.free(id1).unwrap();
        // The next alloc must reuse id1 (it is the lowest free page).
        assert_eq!(fl.alloc(), Some(id1));
        assert_eq!(fl.alloc(), Some(id2 + 1));
    }

    #[test]
    fn test_double_free_is_error() {
        let mut fl = make_fl(64);
        let id = fl.alloc().unwrap();
        fl.free(id).unwrap();
        assert!(fl.free(id).is_err());
    }

    #[test]
    fn test_free_out_of_range_is_error() {
        let mut fl = make_fl(64);
        assert!(fl.free(100).is_err());
    }

    #[test]
    fn test_reserved_pages_never_allocated() {
        let mut fl = make_fl(64);
        let allocated: Vec<u64> = (0..62).map(|_| fl.alloc().unwrap()).collect();
        assert!(!allocated.contains(&0));
        assert!(!allocated.contains(&1));
    }

    #[test]
    fn test_serialization_roundtrip() {
        let mut fl = make_fl(128);
        fl.alloc().unwrap(); // 2
        fl.alloc().unwrap(); // 3
        fl.free(2).unwrap();

        let mut buf = vec![0u8; BITMAP_BODY_BYTES];
        fl.to_bytes(&mut buf);

        let fl2 = FreeList::from_bytes(&buf, 128);
        assert_eq!(fl2.free_count(), fl.free_count());
        // Page 2 is free, page 3 is used.
        let mut fl2 = fl2;
        assert_eq!(fl2.alloc(), Some(2)); // was freed
    }

    #[test]
    fn test_grow_marks_new_pages_free() {
        let mut fl = make_fl(64);
        // Exhaust all pages.
        while fl.alloc().is_some() {}
        assert_eq!(fl.alloc(), None);

        fl.grow(128);
        // After grow, pages 64..128 are free.
        let id = fl.alloc().unwrap();
        assert!((64..128).contains(&id));
    }

    #[test]
    fn test_grow_preserves_used_pages() {
        let mut fl = make_fl(64);
        let ids: Vec<u64> = (0..5).map(|_| fl.alloc().unwrap()).collect();
        fl.grow(128);

        // Already-allocated pages remain USED (not returned by alloc).
        let new_allocs: Vec<u64> = (0..10).map(|_| fl.alloc().unwrap()).collect();
        for id in &ids {
            assert!(
                !new_allocs.contains(id),
                "page {id} already in use reappeared"
            );
        }
    }

    #[test]
    fn test_free_count() {
        let mut fl = make_fl(64);
        assert_eq!(fl.free_count(), 62); // 64 - 2 reserved
        fl.alloc().unwrap();
        assert_eq!(fl.free_count(), 61);
    }

    #[test]
    fn test_cross_word_boundary() {
        // Verify that alloc works correctly crossing the 64-page word boundary.
        let mut fl = make_fl(128);
        // Exhaust the first complete word (pages 2..64).
        for _ in 0..62 {
            fl.alloc().unwrap();
        }
        // The next alloc must jump to the second word.
        let id = fl.alloc().unwrap();
        assert_eq!(id, 64);
    }

    // ── Phase 40.9: batch allocation tests ──────────────────────────────

    #[test]
    fn test_alloc_batch_sequential_contiguous_run() {
        let mut fl = make_fl(256);
        // Pages 0,1 reserved, 2..256 free. Request 64 — should get [2..66).
        let batch = fl.alloc_batch_sequential(64);
        assert_eq!(batch.len(), 64);
        assert_eq!(batch[0], 2);
        assert_eq!(batch[63], 65);
        // All returned IDs are now used.
        for &id in &batch {
            assert!(!fl.is_free(id));
        }
        // Free count decreased by exactly 64.
        assert_eq!(fl.free_count(), 256 - 2 - 64);
    }

    #[test]
    fn test_alloc_batch_sequential_no_contiguous_run_falls_back() {
        let mut fl = make_fl(128);
        // Fragment the bitmap: use every other page from 2..128.
        for i in (2u64..128).step_by(2) {
            fl.mark_used(i);
        }
        // Now no contiguous run of 10 exists — all free pages are odd
        // (3,5,7,...), each separated by a used even page.
        let batch = fl.alloc_batch_sequential(10);
        assert_eq!(batch.len(), 10);
        // All returned IDs are unique and were free.
        let mut sorted = batch.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 10, "no duplicates in non-contiguous batch");
    }

    #[test]
    fn test_alloc_batch_sequential_partial_when_exhausted() {
        let mut fl = make_fl(68); // 68 pages, 0+1 reserved → 66 free.
        let batch = fl.alloc_batch_sequential(100); // ask for 100
        assert_eq!(batch.len(), 66, "should return all 66 free pages");
        assert_eq!(fl.free_count(), 0);
    }

    #[test]
    fn test_alloc_batch_sequential_zero() {
        let mut fl = make_fl(64);
        let batch = fl.alloc_batch_sequential(0);
        assert!(batch.is_empty());
    }

    #[test]
    fn test_free_batch_returns_pages() {
        let mut fl = make_fl(128);
        let batch = fl.alloc_batch_sequential(32);
        assert_eq!(batch.len(), 32);
        let before = fl.free_count();
        fl.free_batch(&batch).unwrap();
        assert_eq!(fl.free_count(), before + 32);
        for &id in &batch {
            assert!(fl.is_free(id));
        }
    }

    #[test]
    fn test_free_batch_double_free_is_error() {
        let mut fl = make_fl(64);
        let id = fl.alloc().unwrap();
        fl.free(id).unwrap();
        // Batch-freeing the same page again should surface an error.
        assert!(fl.free_batch(&[id]).is_err());
    }

    #[test]
    fn test_alloc_batch_sequential_cross_word_contiguous() {
        // Test a contiguous run that spans two 64-bit words.
        let mut fl = make_fl(256);
        // Exhaust pages 2..60 so the run starts at page 60 and spans into word 1.
        for i in 2u64..60 {
            fl.mark_used(i);
        }
        // Free pages: 60..256. A batch of 10 should start at 60 (contiguous).
        let batch = fl.alloc_batch_sequential(10);
        assert_eq!(batch.len(), 10);
        assert_eq!(batch[0], 60);
        assert_eq!(batch[9], 69);
    }
}
