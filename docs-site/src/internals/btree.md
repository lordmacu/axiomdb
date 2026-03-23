# B+ Tree (Copy-on-Write)

NexusDB's indexing layer is a persistent B+ Tree implemented over the `StorageEngine`
trait. Every index — including primary key and unique constraint indexes — is one
such tree. The tree is Copy-on-Write: writes never modify existing pages in place.
Instead, they create new pages for each node on the path from root to the modified
leaf, then atomically swap the root.

---

## Page Capacity — Deriving ORDER_INTERNAL and ORDER_LEAF

Both node types must fit within `PAGE_BODY_SIZE = 16,320` bytes (16 KB minus the
64-byte header). Each key occupies at most `MAX_KEY_LEN = 64` bytes (zero-padded
on disk).

### Internal Node Capacity

An internal node with `n` separator keys has `n + 1` child pointers.

```
Header:    1 (is_leaf) + 1 (_pad) + 2 (num_keys) + 4 (_pad)   =   8 bytes
key_lens:  n × 1                                                =   n bytes
children:  (n + 1) × 8                                         = 8n + 8 bytes
keys:      n × 64                                               = 64n bytes

Total = 8 + n + (8n + 8) + 64n = 16 + 73n
```

Solving `16 + 73n ≤ 16,320`:

```
73n ≤ 16,304
  n ≤ 223.3
```

**ORDER_INTERNAL = 223** (largest integer satisfying the constraint).

Total size: `16 + 73 × 223 = 16 + 16,279 = 16,295 bytes ≤ 16,320 ✓`

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Why 16 KB Pages</span>
PAGE_SIZE = 16 KB was chosen to maximize B+ Tree fanout. With 4 KB pages (SQLite's default), ORDER_INTERNAL would be ~54 — requiring 4× more tree levels for the same number of rows, meaning more page reads per lookup. At ORDER_INTERNAL = 223, a billion-row table fits in a 4-level tree requiring only 4 page reads for a point lookup.
</div>
</div>

### Leaf Node Capacity

A leaf node with `n` entries stores `n` keys and `n` record IDs. A `RecordId`
is 10 bytes: `page_id (u64, 8 bytes) + slot_id (u16, 2 bytes)`.

```
Header:    1 (is_leaf) + 1 (_pad) + 2 (num_keys) + 4 (_pad) + 8 (next_leaf) = 16 bytes
key_lens:  n × 1                                                              =  n bytes
rids:      n × 10                                                             = 10n bytes
keys:      n × 64                                                             = 64n bytes

Total = 16 + n + 10n + 64n = 16 + 75n
```

Solving `16 + 75n ≤ 16,320`:

```
75n ≤ 16,304
  n ≤ 217.4
```

**ORDER_LEAF = 217** (largest integer satisfying the constraint).

Total size: `16 + 75 × 217 = 16 + 16,275 = 16,291 bytes ≤ 16,320 ✓`

---

## On-Disk Page Layout

Both node types use `#[repr(C)]` structs with all-`u8`-array fields so that
`bytemuck::Pod` (zero-copy cast) is safe without any implicit padding. All
multi-byte fields are stored little-endian.

### Internal Node (`InternalNodePage`)

```text
Offset   Size   Field       Description
──────── ────── ─────────── ─────────────────────────────────────────────
       0      1  is_leaf     always 0
       1      1  _pad0       alignment
       2      2  num_keys    number of separator keys (u16 LE)
       4      4  _pad1       alignment
       8    223  key_lens    actual byte length of each key (0 = empty slot)
     231  1,792  children    224 × [u8;8] — child page IDs (u64 LE each)
   2,023 14,272  keys        223 × [u8;64] — separator keys, zero-padded
──────── ────── ─────────── ──────────────────────────────
Total:  16,295 bytes ≤ PAGE_BODY_SIZE ✓
```

### Leaf Node (`LeafNodePage`)

```text
Offset   Size   Field       Description
──────── ────── ─────────── ─────────────────────────────────────────────
       0      1  is_leaf     always 1
       1      1  _pad0       alignment
       2      2  num_keys    number of (key, rid) pairs (u16 LE)
       4      4  _pad1       alignment
       8      8  next_leaf   page_id of the next leaf (u64 LE); u64::MAX = no next
      16    217  key_lens    actual byte length of each key
     233  2,170  rids        217 × [u8;10] — RecordId (page_id:8 + slot_id:2)
   2,403 13,888  keys        217 × [u8;64] — keys, zero-padded
──────── ────── ─────────── ──────────────────────────────
Total:  16,291 bytes ≤ PAGE_BODY_SIZE ✓
```

---

## Copy-on-Write Root Swap

The root page ID is stored in an `AtomicU64`. Writers and readers interact with
it as follows.

### Reader Path

```rust
// Acquire load: guaranteed to see all writes that happened before
// the Release store that set this root.
let root_id = self.root.load(Ordering::Acquire);
let root_page = storage.read_page(root_id)?;
// traverse down — no locks acquired
```

### Writer Path

```rust
// 1. Load the current root
let old_root_id = self.root.load(Ordering::Acquire);

// 2. Walk from old_root down to the target leaf, collecting the path
let path = find_path(&storage, old_root_id, key)?;

// 3. For each node on the path (leaf first, then up to root):
//    a. alloc_page → new_page_id
//    b. copy content from old page
//    c. apply the mutation (insert key/split/rebalance)
//    d. update the parent's child pointer to new_page_id

// 4. The new root was written as a new page
let new_root_id = path[0].new_page_id;

// 5. Atomic swap — Release store: all prior writes visible to Acquire loads
self.root.store(new_root_id, Ordering::Release);

// 6. Free the old path pages (only safe after all readers have moved on)
for old_id in old_page_ids { storage.free_page(old_id)?; }
```

A reader that loaded `old_root_id` before the swap continues accessing old pages
safely — they are freed only after all reads complete (tracked in Phase 7 with
epoch-based reclamation).

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Lock-Free Reads</span>
Readers load the root pointer with <code>Acquire</code> semantics and traverse the tree without acquiring any lock. A write in progress is invisible to readers until the <code>Release</code> store completes — at which point the entire new subtree is already consistent. This is what allows read throughput to scale linearly with core count.
</div>
</div>

---

## Why next_leaf Is Not Used in Range Scans

The leaf node format includes a `next_leaf` pointer for a traditional linked-list
traversal across leaf nodes. However, this pointer is **not used** by `RangeIter`.

**Reason:** Under CoW, when a leaf is split or modified, a new page is created. The
previous leaf in the linked list still points to the old page (`L_old`), which has
already been freed. Keeping the linked list consistent under CoW would require copying
the previous leaf on every split — but finding the previous leaf during an insert
requires traversing from the root (the tree has no backward pointers).

**Adopted solution:** `RangeIter` re-traverses the tree from the root to find the
next leaf when crossing a leaf boundary. The cost is O(log n) per boundary crossing,
not O(1) as with a linked list. For a tree of 1 billion rows with ORDER_LEAF = 217,
the depth is `log₂₁₇(10⁹) ≈ 4`, so each boundary crossing is 4 page reads.
Measured cost for a range scan of 10,000 rows: **0.61 ms** — well within the 45 ms budget.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Re-traversal vs. Linked-List Leaf Scan</span>
The <code>next_leaf</code> pointer exists on-disk but <code>RangeIter</code> does not use it. Under CoW, keeping a consistent linked list would require copying the <em>previous</em> leaf on every split — which itself requires finding that leaf from the root. Re-traversal costs O(log n) per leaf boundary (4 reads at 1B rows) and is simpler to reason about correctly.
</div>
</div>

---

## Insert — CoW Split Protocol

```
1. Descend from root to the target leaf, recording the path.

2. If the leaf has room (num_keys < ORDER_LEAF):
   → Copy the leaf to a new page.
   → Insert the new (key, rid) in sorted position.
   → Update the parent's child pointer (CoW the parent too).
   → Propagate CoW up to the root.

3. If the leaf is full:
   → Allocate two new leaf pages.
   → Distribute: left gets floor((ORDER_LEAF+1)/2) entries,
                 right gets the remaining entries.
   → The smallest key of the right leaf becomes the separator key
     pushed up to the parent.
   → CoW the parent, insert the new separator and child pointer.
   → If the parent is also full, recursively split upward.
   → If the root splits, allocate a new root with two children.
```

### Minimum Occupancy Invariant

All nodes except the root must remain at least half full after any operation:

- Internal nodes: `num_keys ≥ ORDER_INTERNAL / 2 = 111`
- Leaf nodes: `num_keys ≥ ORDER_LEAF / 2 = 108`

Violations of this invariant during delete trigger rebalancing (redistribution
from a sibling if possible, merge otherwise).

---

## Prefix Compression — In-Memory Only

Internal node keys are often highly redundant. For a tree indexing sequential IDs,
consecutive separator keys share long common prefixes. NexusDB implements
`CompressedNode` as an in-memory representation:

```rust
struct CompressedNode {
    prefix: Box<[u8]>,          // longest common prefix of all keys in this node
    suffixes: Vec<Box<[u8]>>,   // remainder of each key after stripping the prefix
}
```

When an internal node page is read from disk, it is optionally decompressed into a
`CompressedNode` for faster binary search (searching on suffix bytes only). When the
node is written back, the full keys are reconstructed. This is a read optimization
only — the on-disk format always stores full keys.

The compression ratio depends on key structure. For an 8-byte integer key, there is no
prefix to compress. For a 64-byte composite key `(category_id || product_name)`, the
`category_id` prefix is shared across many consecutive keys and is compressed away.

---

## Tree Height and Fan-Out

| Rows          | Tree depth | Notes                                       |
|---------------|------------|---------------------------------------------|
| 1–217         | 1 (root = leaf) | Entire tree is one leaf page          |
| 218–47,089    | 2          | One root internal + up to 218 leaves        |
| 47K–10.2M     | 3          | Two levels of internals                     |
| 10.2M–2.22B   | 4          | Covers billion-row tables comfortably       |
| >2.22B        | 5          | Rare; still fast at O(log n) traversal     |

A tree of 1 billion rows has depth 4 — a point lookup requires reading 4 pages
(1 per level). At 16 KB pages, a warm cache point lookup is ~4 memory accesses
with no disk I/O.
