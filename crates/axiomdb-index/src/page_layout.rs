//! B+ Tree node layout in 16 KB pages.
//!
//! All fields are `u8` arrays (alignment 1) to guarantee that
//! `bytemuck::Pod` works without implicit padding or alignment issues.
//! Multi-byte values are stored in little-endian.
//!
//! ## Layout constants
//!
//! `PAGE_BODY_SIZE = 16_320` bytes available.
//!
//! ### Internal node (`ORDER_INTERNAL = 223`)
//! ```text
//! [header:    8 B]  is_leaf=0 | _pad | num_keys([u8;2]) | _pad([u8;4])
//! [key_lens: 223 B] actual length of each key (0 = empty slot)
//! [children: 1792 B] (223+1) page pointers, 8 bytes each
//! [keys:   14272 B] 223 × 64 bytes, zero-padded to MAX_KEY_LEN
//! Total: 16295 ≤ 16320 ✓
//! ```
//!
//! ### Leaf node (`ORDER_LEAF = 217`)
//! ```text
//! [header:    16 B]  is_leaf=1 | _pad | num_keys([u8;2]) | _pad([u8;4]) | next_leaf([u8;8])
//! [key_lens: 217 B]  actual length of each key
//! [rids:    2170 B]  217 × 10 bytes: page_id(8 LE) + slot_id(2 LE)
//! [keys:   13888 B]  217 × 64 bytes, zero-padded
//! Total: 16291 ≤ 16320 ✓
//! ```

use std::mem::size_of;

use axiomdb_core::RecordId;

// ── Public constants ──────────────────────────────────────────────────────────

/// Maximum length of a key in bytes.
pub const MAX_KEY_LEN: usize = 64;

/// Maximum number of keys in an internal node.
pub const ORDER_INTERNAL: usize = 223;

/// Maximum number of keys in a leaf node.
pub const ORDER_LEAF: usize = 217;

/// Size of a page body (PAGE_SIZE - HEADER_SIZE).
pub const PAGE_BODY_SIZE: usize = axiomdb_storage::PAGE_SIZE - axiomdb_storage::HEADER_SIZE;

/// Sentinel: no next leaf / no child.
pub const NULL_PAGE: u64 = u64::MAX;

/// Minimum keys in an internal node (except the root).
pub const MIN_KEYS_INTERNAL: usize = ORDER_INTERNAL / 2;

/// Minimum keys in a leaf node (except when it is also the root).
pub const MIN_KEYS_LEAF: usize = ORDER_LEAF / 2;

// ── Internal Node ─────────────────────────────────────────────────────────────

/// Binary representation of an internal node in the body of a page.
///
/// All fields are `[u8; N]` arrays → alignment = 1, no implicit padding.
///
/// Layout (16295 bytes):
/// ```text
/// Offset    Size    Field
///      0         1  is_leaf  (always 0)
///      1         1  _pad0
///      2         2  num_keys  (LE u16)
///      4         4  _pad1
///      8       223  key_lens  (1 byte per key: actual length)
///    231      1792  children  (224 × [u8;8], LE u64 per entry)
///   2023     14272  keys      (223 × [u8;64], zero-padded)
/// Total: 16295
/// ```
#[repr(C)]
#[derive(Clone, Copy)]
pub struct InternalNodePage {
    pub is_leaf: u8,
    pub _pad0: u8,
    pub num_keys: [u8; 2],
    pub _pad1: [u8; 4],
    pub key_lens: [u8; ORDER_INTERNAL],
    pub children: [[u8; 8]; ORDER_INTERNAL + 1],
    pub keys: [[u8; MAX_KEY_LEN]; ORDER_INTERNAL],
}

const _: () = assert!(
    size_of::<InternalNodePage>()
        == 8 + ORDER_INTERNAL + (ORDER_INTERNAL + 1) * 8 + ORDER_INTERNAL * MAX_KEY_LEN,
    "InternalNodePage: incorrect size"
);
const _: () = assert!(
    size_of::<InternalNodePage>() <= PAGE_BODY_SIZE,
    "InternalNodePage does not fit in the page body"
);

// SAFETY: InternalNodePage is #[repr(C)] with all fields being u8 arrays.
// - No implicit padding (alignment = 1, all fields are u8 or [u8;N]).
// - Any bit pattern is a valid value (all fields are u8).
// - The size is exactly the sum of the fields (verified by the assert above).
unsafe impl bytemuck::Zeroable for InternalNodePage {}
unsafe impl bytemuck::Pod for InternalNodePage {}

impl InternalNodePage {
    pub fn num_keys(&self) -> usize {
        u16::from_le_bytes(self.num_keys) as usize
    }

    pub fn set_num_keys(&mut self, n: usize) {
        self.num_keys = (n as u16).to_le_bytes();
    }

    pub fn key_at(&self, i: usize) -> &[u8] {
        let len = self.key_lens[i] as usize;
        debug_assert!(
            len <= MAX_KEY_LEN,
            "corrupt InternalNodePage: key_lens[{i}]={len} > MAX_KEY_LEN={MAX_KEY_LEN}, num_keys={}",
            self.num_keys()
        );
        &self.keys[i][..len]
    }

    pub fn set_key_at(&mut self, i: usize, k: &[u8]) {
        debug_assert!(k.len() <= MAX_KEY_LEN);
        self.key_lens[i] = k.len() as u8;
        self.keys[i][..k.len()].copy_from_slice(k);
        // Clear remaining bytes to avoid stale data
        self.keys[i][k.len()..].fill(0);
    }

    pub fn child_at(&self, i: usize) -> u64 {
        u64::from_le_bytes(self.children[i])
    }

    pub fn set_child_at(&mut self, i: usize, pid: u64) {
        self.children[i] = pid.to_le_bytes();
    }

    /// Index of the child to follow for the given key (binary search).
    /// Returns index `j` such that `children[j]` contains the range for `key`.
    ///
    /// Finds the first separator strictly greater than `key` using binary search.
    /// Keys are sorted by the B+ Tree invariant → O(log n) comparisons.
    /// Validates that all key_lens[0..num_keys] are ≤ MAX_KEY_LEN.
    /// Panics in debug builds when a corrupted node is detected.
    #[cfg(debug_assertions)]
    pub fn validate(&self) {
        let n = self.num_keys();
        for i in 0..n {
            let len = self.key_lens[i] as usize;
            assert!(
                len <= MAX_KEY_LEN,
                "corrupt InternalNodePage at validate: key_lens[{i}]={len} > MAX_KEY_LEN={MAX_KEY_LEN}, num_keys={n}"
            );
        }
    }

    pub fn find_child_idx(&self, key: &[u8]) -> usize {
        let n = self.num_keys();
        let mut lo = 0usize;
        let mut hi = n;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.key_at(mid) <= key {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// Inserts a (sep_key, right_child_pid) pair at position `pos`.
    /// Shifts existing entries to the right. Increments num_keys.
    ///
    /// Precondition: num_keys < ORDER_INTERNAL
    pub fn insert_at(&mut self, pos: usize, sep_key: &[u8], right_pid: u64) {
        let n = self.num_keys();
        debug_assert!(n < ORDER_INTERNAL, "internal node full before insert");

        // Shift keys and key_lens
        for i in (pos..n).rev() {
            self.keys[i + 1] = self.keys[i];
            self.key_lens[i + 1] = self.key_lens[i];
        }
        // Shift children (children[pos+1..=n+1])
        for i in (pos..=n).rev() {
            let pid = self.child_at(i);
            self.set_child_at(i + 1, pid);
        }

        self.set_key_at(pos, sep_key);
        self.set_child_at(pos + 1, right_pid);
        self.set_num_keys(n + 1);
    }

    /// Removes the key at position `key_pos` and the child at `child_pos`.
    /// Used during merge: removes the separator and one of the merged children.
    pub fn remove_at(&mut self, key_pos: usize, child_pos: usize) {
        let n = self.num_keys();
        debug_assert!(n > 0);
        debug_assert!(key_pos < n);
        debug_assert!(child_pos <= n);

        for i in key_pos..n - 1 {
            self.keys[i] = self.keys[i + 1];
            self.key_lens[i] = self.key_lens[i + 1];
        }
        self.key_lens[n - 1] = 0;
        self.keys[n - 1].fill(0);

        for i in child_pos..n {
            let pid = self.child_at(i + 1);
            self.set_child_at(i, pid);
        }
        self.set_child_at(n, 0);
        self.set_num_keys(n - 1);
    }
}

// ── Leaf Node ─────────────────────────────────────────────────────────────────

/// Binary representation of a leaf node in the body of a page.
///
/// Layout (16291 bytes):
/// ```text
/// Offset    Size    Field
///      0         1  is_leaf  (always 1)
///      1         1  _pad0
///      2         2  num_keys  (LE u16)
///      4         4  _pad1
///      8         8  next_leaf  (LE u64, NULL_PAGE if this is the last leaf)
///     16       217  key_lens
///    233      2170  rids      (217 × [u8;10]: page_id(8 LE) + slot_id(2 LE))
///   2403     13888  keys      (217 × [u8;64], zero-padded)
/// Total: 16291
/// ```
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LeafNodePage {
    pub is_leaf: u8,
    pub _pad0: u8,
    pub num_keys: [u8; 2],
    pub _pad1: [u8; 4],
    pub next_leaf: [u8; 8],
    pub key_lens: [u8; ORDER_LEAF],
    pub rids: [[u8; 10]; ORDER_LEAF],
    pub keys: [[u8; MAX_KEY_LEN]; ORDER_LEAF],
}

const _: () = assert!(
    size_of::<LeafNodePage>() == 16 + ORDER_LEAF + ORDER_LEAF * 10 + ORDER_LEAF * MAX_KEY_LEN,
    "LeafNodePage: incorrect size"
);
const _: () = assert!(
    size_of::<LeafNodePage>() <= PAGE_BODY_SIZE,
    "LeafNodePage does not fit in the page body"
);

// SAFETY: LeafNodePage is #[repr(C)] with all fields being u8 arrays.
// - No implicit padding (alignment = 1).
// - Any bit pattern is valid (all fields are u8 or [u8;N]).
unsafe impl bytemuck::Zeroable for LeafNodePage {}
unsafe impl bytemuck::Pod for LeafNodePage {}

impl LeafNodePage {
    pub fn num_keys(&self) -> usize {
        u16::from_le_bytes(self.num_keys) as usize
    }

    pub fn set_num_keys(&mut self, n: usize) {
        self.num_keys = (n as u16).to_le_bytes();
    }

    pub fn next_leaf_val(&self) -> u64 {
        u64::from_le_bytes(self.next_leaf)
    }

    pub fn set_next_leaf(&mut self, pid: u64) {
        self.next_leaf = pid.to_le_bytes();
    }

    pub fn key_at(&self, i: usize) -> &[u8] {
        &self.keys[i][..self.key_lens[i] as usize]
    }

    pub fn set_key_at(&mut self, i: usize, k: &[u8]) {
        debug_assert!(k.len() <= MAX_KEY_LEN);
        self.key_lens[i] = k.len() as u8;
        self.keys[i][..k.len()].copy_from_slice(k);
        self.keys[i][k.len()..].fill(0);
    }

    pub fn rid_at(&self, i: usize) -> RecordId {
        decode_rid(self.rids[i])
    }

    pub fn set_rid_at(&mut self, i: usize, rid: RecordId) {
        self.rids[i] = encode_rid(rid);
    }

    /// Binary search for `key` in the leaf node.
    /// Returns `Ok(idx)` if found, `Err(idx)` with insertion position if not.
    pub fn search(&self, key: &[u8]) -> Result<usize, usize> {
        let n = self.num_keys();
        let mut lo = 0usize;
        let mut hi = n;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match self.key_at(mid).cmp(key) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Ok(mid),
            }
        }
        Err(lo)
    }

    /// Inserts (key, rid) at position `pos`. Shifts existing entries.
    /// Precondition: num_keys < ORDER_LEAF
    pub fn insert_at(&mut self, pos: usize, key: &[u8], rid: RecordId) {
        let n = self.num_keys();
        debug_assert!(n < ORDER_LEAF);

        for i in (pos..n).rev() {
            self.keys[i + 1] = self.keys[i];
            self.key_lens[i + 1] = self.key_lens[i];
            self.rids[i + 1] = self.rids[i];
        }
        self.set_key_at(pos, key);
        self.set_rid_at(pos, rid);
        self.set_num_keys(n + 1);
    }

    /// Removes the entry at position `pos`. Shifts remaining entries.
    pub fn remove_at(&mut self, pos: usize) {
        let n = self.num_keys();
        debug_assert!(pos < n);

        for i in pos..n - 1 {
            self.keys[i] = self.keys[i + 1];
            self.key_lens[i] = self.key_lens[i + 1];
            self.rids[i] = self.rids[i + 1];
        }
        self.key_lens[n - 1] = 0;
        self.keys[n - 1].fill(0);
        self.rids[n - 1] = [0u8; 10];
        self.set_num_keys(n - 1);
    }
}

// ── RecordId serialization helpers ───────────────────────────────────────────

/// Serializes RecordId to 10 bytes: page_id (8 LE) + slot_id (2 LE).
#[inline]
pub fn encode_rid(rid: RecordId) -> [u8; 10] {
    let mut buf = [0u8; 10];
    buf[..8].copy_from_slice(&rid.page_id.to_le_bytes());
    buf[8..10].copy_from_slice(&rid.slot_id.to_le_bytes());
    buf
}

/// Deserializes RecordId from 10 bytes.
///
/// Uses direct array indexing to avoid `try_into().unwrap()` in production code
/// — the compiler verifies the sizes at compile time.
#[inline]
pub fn decode_rid(buf: [u8; 10]) -> RecordId {
    RecordId {
        page_id: u64::from_le_bytes([
            buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
        ]),
        slot_id: u16::from_le_bytes([buf[8], buf[9]]),
    }
}

// ── Zero-copy casts ───────────────────────────────────────────────────────────

/// Gets an immutable reference to the internal node from the page body.
///
/// # SAFETY via bytemuck
/// `InternalNodePage` is `Pod` (all bytes valid for any bit pattern).
/// The body has `PAGE_BODY_SIZE >= size_of::<InternalNodePage>()` bytes.
/// Verified with `assert_eq!(page.body()[0], 0)` before calling in production.
pub fn cast_internal(page: &axiomdb_storage::Page) -> &InternalNodePage {
    bytemuck::from_bytes(&page.body()[..size_of::<InternalNodePage>()])
}

/// Gets a mutable reference to the internal node from the page body.
pub fn cast_internal_mut(page: &mut axiomdb_storage::Page) -> &mut InternalNodePage {
    bytemuck::from_bytes_mut(&mut page.body_mut()[..size_of::<InternalNodePage>()])
}

/// Gets an immutable reference to the leaf node from the page body.
pub fn cast_leaf(page: &axiomdb_storage::Page) -> &LeafNodePage {
    bytemuck::from_bytes(&page.body()[..size_of::<LeafNodePage>()])
}

/// Gets a mutable reference to the leaf node from the page body.
pub fn cast_leaf_mut(page: &mut axiomdb_storage::Page) -> &mut LeafNodePage {
    bytemuck::from_bytes_mut(&mut page.body_mut()[..size_of::<LeafNodePage>()])
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use bytemuck::Zeroable;

    #[test]
    fn test_internal_node_size_fits_page() {
        assert!(size_of::<InternalNodePage>() <= PAGE_BODY_SIZE);
        // Verify the exact value calculated in the spec
        assert_eq!(
            size_of::<InternalNodePage>(),
            8 + ORDER_INTERNAL + (ORDER_INTERNAL + 1) * 8 + ORDER_INTERNAL * MAX_KEY_LEN
        );
    }

    #[test]
    fn test_leaf_node_size_fits_page() {
        assert!(size_of::<LeafNodePage>() <= PAGE_BODY_SIZE);
        assert_eq!(
            size_of::<LeafNodePage>(),
            16 + ORDER_LEAF + ORDER_LEAF * 10 + ORDER_LEAF * MAX_KEY_LEN
        );
    }

    #[test]
    fn test_rid_encode_decode_roundtrip() {
        let rid = RecordId {
            page_id: 0xDEADBEEF_CAFE1234,
            slot_id: 0xABCD,
        };
        let encoded = encode_rid(rid);
        let decoded = decode_rid(encoded);
        assert_eq!(decoded.page_id, rid.page_id);
        assert_eq!(decoded.slot_id, rid.slot_id);
    }

    #[test]
    fn test_internal_node_key_ops() {
        let mut node = InternalNodePage::zeroed();
        node.is_leaf = 0;
        node.set_num_keys(0);

        // Manually insert 3 children and 2 separators
        node.set_child_at(0, 100);
        node.insert_at(0, b"key_b", 200);
        node.insert_at(1, b"key_d", 300);
        // Now: children=[100, 200, 300], keys=["key_b", "key_d"]
        assert_eq!(node.num_keys(), 2);
        assert_eq!(node.key_at(0), b"key_b");
        assert_eq!(node.key_at(1), b"key_d");
        assert_eq!(node.child_at(0), 100);
        assert_eq!(node.child_at(1), 200);
        assert_eq!(node.child_at(2), 300);
    }

    #[test]
    fn test_internal_find_child_idx() {
        let mut node = InternalNodePage::zeroed();
        node.set_num_keys(0);
        node.set_child_at(0, 10);
        node.insert_at(0, b"key_b", 20);
        node.insert_at(1, b"key_d", 30);
        // keys=["key_b","key_d"], children=[10,20,30]

        // key < "key_b" → idx=0
        assert_eq!(node.find_child_idx(b"aaa"), 0);
        // key == "key_b" → idx=1 (first separator > "key_b" is "key_d" at i=1)
        assert_eq!(node.find_child_idx(b"key_b"), 1);
        // key between "key_b" and "key_d"
        assert_eq!(node.find_child_idx(b"key_c"), 1);
        // key == "key_d" → idx=2
        assert_eq!(node.find_child_idx(b"key_d"), 2);
        // key > "key_d" → idx=2 (num_keys=2)
        assert_eq!(node.find_child_idx(b"zzz"), 2);
    }

    #[test]
    fn test_leaf_node_insert_remove() {
        let mut node = LeafNodePage::zeroed();
        node.is_leaf = 1;
        node.set_next_leaf(NULL_PAGE);

        let rid1 = RecordId {
            page_id: 1,
            slot_id: 0,
        };
        let rid2 = RecordId {
            page_id: 2,
            slot_id: 1,
        };

        assert_eq!(node.search(b"aaa"), Err(0));
        node.insert_at(0, b"bbb", rid1);
        assert_eq!(node.num_keys(), 1);
        assert_eq!(node.key_at(0), b"bbb");
        assert_eq!(node.rid_at(0).page_id, 1);

        // Insert before
        node.insert_at(0, b"aaa", rid2);
        assert_eq!(node.num_keys(), 2);
        assert_eq!(node.key_at(0), b"aaa");
        assert_eq!(node.key_at(1), b"bbb");

        // Remove the first
        node.remove_at(0);
        assert_eq!(node.num_keys(), 1);
        assert_eq!(node.key_at(0), b"bbb");
    }

    #[test]
    fn test_leaf_search() {
        let mut node = LeafNodePage::zeroed();
        node.is_leaf = 1;
        let rid = RecordId {
            page_id: 1,
            slot_id: 0,
        };
        node.insert_at(0, b"ccc", rid);
        node.insert_at(1, b"eee", rid);
        node.insert_at(2, b"ggg", rid);

        assert_eq!(node.search(b"aaa"), Err(0)); // before ccc
        assert_eq!(node.search(b"ccc"), Ok(0));
        assert_eq!(node.search(b"ddd"), Err(1)); // between ccc and eee
        assert_eq!(node.search(b"eee"), Ok(1));
        assert_eq!(node.search(b"ggg"), Ok(2));
        assert_eq!(node.search(b"zzz"), Err(3)); // after ggg
    }
}
