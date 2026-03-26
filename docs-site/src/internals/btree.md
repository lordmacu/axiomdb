# B+ Tree (Copy-on-Write)

AxiomDB's indexing layer is a persistent B+ Tree implemented over the `StorageEngine`
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

2. If the leaf has room (num_keys < fill_threshold):
   → Copy the leaf to a new page.
   → Insert the new (key, rid) in sorted position.
   → Update the parent's child pointer (CoW the parent too).
   → Propagate CoW up to the root.

3. If the leaf is at or above the fill threshold:
   → Allocate two new leaf pages.
   → Distribute: left gets floor((ORDER_LEAF+1)/2) entries,
                 right gets the remaining entries.
   → The smallest key of the right leaf becomes the separator key
     pushed up to the parent.
   → CoW the parent, insert the new separator and child pointer.
   → If the parent is also full, recursively split upward.
   → If the root splits, allocate a new root with two children.
```

The split point `fill_threshold` depends on the index fill factor (see below).
Internal pages always split at `ORDER_INTERNAL` regardless of fill factor.

---

## Fill Factor — Adaptive Leaf Splits

The fill factor controls how full leaf pages are allowed to get before splitting.
It is set per-index via `WITH (fillfactor=N)` on `CREATE INDEX` and stored in
`IndexDef.fillfactor: u8`.

### Formula

```
fill_threshold(order, ff) = ⌈order × ff / 100⌉   (integer ceiling division)
```

| fillfactor | fill_threshold (ORDER_LEAF = 216) | Effect |
|---|---|---|
| 100 (compact) | 216 | Splits only when completely full — max density, slowest inserts on busy pages |
| 90 (default)  | 195 | Leaves ~10% free — balances density and insert speed |
| 70 (write-heavy) | 152 | Leaves ~30% free — fewer splits for append-heavy workloads |
| 10 (minimum)  | 22  | Very sparse pages — extreme fragmentation, rarely useful |

A compile-time assert verifies that `fill_threshold(ORDER_LEAF, 100) == ORDER_LEAF`,
ensuring fillfactor=100 always preserves the original behavior exactly.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision</span>
Internal pages are <strong>not</strong> affected by fill factor — they always split at
<code>ORDER_INTERNAL</code>. Only leaf splits benefit from the extra free space, because
inserts always land on leaf pages. Applying fill factor to internal pages would reduce
tree fan-out without any benefit for typical insert patterns, matching PostgreSQL's
implementation of the same concept.
</div>
</div>

### Catalog field

`IndexDef.fillfactor` is serialized as a single byte appended after the predicate
section in the catalog heap entry. Pre-6.8 index rows are read with a default of 90
(backward-compatible). Valid range: 10–100; values outside this range are rejected
at `CREATE INDEX` parse time with a `ParseError`.

### When to use a lower fill factor

- **Append-heavy tables** — rows inserted in bulk after the index is created. A
  fill factor of 70–80 prevents cascading splits during the bulk load.
- **Write-heavy OLTP** — high-frequency single-row inserts that land on the same
  hot pages. More free space means fewer page splits per second.
- **Read-heavy / archival** — use fillfactor=100. Maximum density reduces I/O for
  full scans at the cost of slower writes.

### Minimum Occupancy Invariant

All nodes except the root must remain at least half full after any operation:

- Internal nodes: `num_keys ≥ ORDER_INTERNAL / 2 = 111`
- Leaf nodes: `num_keys ≥ ORDER_LEAF / 2 = 108`

Violations of this invariant during delete trigger rebalancing (redistribution
from a sibling if possible, merge otherwise).

#### `rotate_right` key-shift invariant

When `rotate_right` borrows the last key of the left sibling and inserts it at
position 0 of the underfull child (internal node case), all existing keys in the
child must be shifted right by one position **before** inserting the new key.

The shift must cover positions `0..cn` → `1..cn+1`, implemented with a reverse
loop (same pattern as `insert_at`). Using `slice::rotate_right(1)` on `[..cn]`
is incorrect: it moves `key[cn-1]` to position 0 where it is immediately
overwritten, leaving position `cn` with stale data from a previous operation.
The stale byte can exceed `MAX_KEY_LEN = 64`, causing a bounds panic on the next
traversal of that node.

```rust
// Correct: explicit reverse loop
for i in (0..cn).rev() {
    child.key_lens[i + 1] = child.key_lens[i];
    child.keys[i + 1]     = child.keys[i];
}
child.key_lens[0] = sep_len;
child.keys[0]     = sep_key;
```

---

## Prefix Compression — In-Memory Only

Internal node keys are often highly redundant. For a tree indexing sequential IDs,
consecutive separator keys share long common prefixes. AxiomDB implements
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

---

## Static API — Shared-Storage Operations (Phase 6.2)

`BTree` normally owns its `Box<dyn StorageEngine>`. This is convenient for tests but
prevents sharing one `MmapStorage` between the table heap and multiple indexes. Phase
6.2 adds static functions that accept an external `&mut dyn StorageEngine`:

```rust
// Point lookup — read-only, no ownership needed
BTree::lookup_in(storage: &dyn StorageEngine, root_pid: u64, key: &[u8])
    -> Result<Option<RecordId>, DbError>

// Insert — mutates storage, updates root_pid atomically on root split
BTree::insert_in(storage: &mut dyn StorageEngine, root_pid: &AtomicU64, key: &[u8], rid: RecordId)
    -> Result<(), DbError>

// Delete — mutates storage, updates root_pid atomically on root collapse
BTree::delete_in(storage: &mut dyn StorageEngine, root_pid: &AtomicU64, key: &[u8])
    -> Result<bool, DbError>

// Range scan — collects all (RecordId, key_bytes) in [lo, hi] into a Vec
BTree::range_in(storage: &dyn StorageEngine, root_pid: u64, lo: Option<&[u8]>, hi: Option<&[u8]>)
    -> Result<Vec<(RecordId, Vec<u8>)>, DbError>
```

These delegate to the same private helpers as the owned API. The `insert_in` and
`delete_in` variants use `AtomicU64::store(Release)` instead of `compare_exchange`
(safe in Phase 6 — single writer).

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision</span>
`range_in` returns `Vec<(RecordId, Vec<u8>)>` rather than an iterator to avoid
lifetime conflicts between the borrow of storage needed to drive the iterator and the
caller's existing `&mut storage` borrow. The heap reads happen after the range scan
completes, which requires full ownership of the results.
</div>
</div>

---

## Order-Preserving Key Encoding (Phase 6.1b)

Secondary index keys are encoded as byte slices in `axiomdb-sql/src/key_encoding.rs`
such that `encode(a) < encode(b)` iff `a < b` under SQL comparison semantics. Each
`Value` variant is prefixed with a 1-byte type tag:

| Type | Tag | Payload | Order property |
|------|-----|---------|----------------|
| `NULL` | 0x00 | none | Sorts before all non-NULL |
| `Bool` | 0x01 | 1 byte | false < true |
| `Int(i32)` | 0x02 | 8 BE bytes after `n ^ i64::MIN` | Negative < positive |
| `BigInt(i64)` | 0x03 | 8 BE bytes after `n ^ i64::MIN` | Negative < positive |
| `Real(f64)` | 0x04 | 8 bytes (NaN=0, pos=MSB set, neg=all flipped) | IEEE order |
| `Decimal(i128, u8)` | 0x05 | 1 (scale) + 16 BE bytes after sign-flip | |
| `Date(i32)` | 0x06 | 8 BE bytes after sign-flip | |
| `Timestamp(i64)` | 0x07 | 8 BE bytes after sign-flip | Older < newer |
| `Text` | 0x08 | NUL-terminated UTF-8, 0x00 escaped as `[0xFF, 0x00]` | Lexicographic |
| `Bytes` | 0x09 | NUL-terminated, same escape | Lexicographic |
| `Uuid` | 0x0A | 16 raw bytes | Lexicographic |

For composite keys the encodings are concatenated — the first column has the most
significant sort influence.

**NULL handling**: NULL values are not inserted into secondary index B-Trees. This is
consistent with SQL semantics (`NULL ≠ NULL`) and avoids DuplicateKey errors when
multiple NULLs appear in a UNIQUE index. `WHERE col = NULL` always falls through to a
full scan.

**Maximum key length**: 768 bytes. Keys exceeding this return `DbError::IndexKeyTooLong`
and are silently skipped during `CREATE INDEX`.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision</span>
Integer sign-flip (`n ^ i64::MIN`) converts a signed two's-complement integer into
an unsigned value that sorts in the same order. This is the same technique used by
RocksDB's `WriteBatchWithIndex`, CockroachDB's key encoding, and PostgreSQL's
`btint4cmp` — proven correct and branch-free at O(1).
</div>
</div>
