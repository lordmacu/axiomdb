# Spec: 40.8 — B-Tree Latch Coupling

## What to build (not how)

Make B-tree index operations (search, insert, delete, range scan) safe for concurrent
transactions using a **hybrid optimistic/pessimistic latch coupling protocol** inspired
by InnoDB's three-mode latch system. This is the most complex subfase in Phase 40 —
a bug here means silent index corruption under concurrency.

## Research findings

### InnoDB latch protocol (primary reference — battle-tested at billions of txns)

**Three latch modes for tree traversal** (btr0types.h):
- **BTR_SEARCH_LEAF** (S-latch): read-only descent, S-latch leaf only
- **BTR_MODIFY_LEAF** (X-latch leaf): optimistic write, X-latch only at leaf
- **BTR_MODIFY_TREE** (X-latch tree): pessimistic, X-latch from root through all levels

**Descent algorithm** (`btr_cur_t::open_leaf()`, btr0cur.cc:1866-2088):
1. Acquire **SX-latch on index lock** (tree-level intention lock)
2. Descend level by level, acquiring S-latch on each internal page
3. At each level: acquire child latch BEFORE releasing parent (coupling)
4. At leaf: acquire appropriate mode (S for read, X for write)
5. Release all non-leaf latches (kept in MTR savepoint stack)
6. If leaf operation fails (page full) → restart with BTR_MODIFY_TREE

**Split latch protocol** (`btr_attach_half_pages()`, btr0btr.cc:2420-2576):
- During leaf split: X-latch on current page + new page + previous sibling + parent
- **Latch order**: LEFT sibling → current → RIGHT sibling → PARENT (ascending block number)
- Parent insertion may cascade split upward — same protocol recursively

**Key innovation: SX-latch** (shared-exclusive):
- Compatible with S-latches (readers continue during write preparation)
- Not compatible with X or other SX (serializes writers at tree level)
- Enables optimistic protocol at root with reduced contention vs full X-latch

### PostgreSQL Lehman-Yao protocol (reference for move-right recovery)

**Move-right rule** (nbtree/README:14-124):
- Every page has a **right-link** to its right sibling
- Every page has a **high key** (upper bound of keys on this page)
- If search key > high_key → page was split concurrently → follow right-link
- Readers need NO parent latch — right-link provides recovery from concurrent splits

**Incomplete split flag** (README:667-681):
- After split, left page marked `BTP_INCOMPLETE_SPLIT`
- Right page has no parent downlink yet
- Next insert/vacuum lazily completes the split by inserting downlink
- Eliminates need to hold parent latch during split (reduces contention window)

**Split WAL logging**: two records (split + parent insert). Between records,
tree is in "incomplete" state but still navigable via right-links.

### AxiomDB current B-tree architecture

**Index B-tree** (tree.rs): Fixed-slot LeafNodePage/InternalNodePage with bytemuck Pod.
Recursive insert_subtree/delete_subtree. Copy-on-Write with atomic root CAS.
**NO latches today** — assumes single writer.

**Clustered B-tree** (clustered_tree.rs): Variable-size ClusteredLeafPage with cell
pointer array. Similar recursive descent. **NO latches today**.

**Key state during split:**
- Allocates 2 new pages (left + right) via alloc_page
- Writes both pages
- Frees old page
- Returns InsertResult::Split to parent for separator insertion
- Parent may cascade split

**Pages held simultaneously during split:** 3 (old + left + right in memory copies,
not latched). With latching: must hold X-latch on all 3 + parent during split.

### Design choice for AxiomDB: InnoDB hybrid + Lehman-Yao right-link

**Why hybrid (not pure InnoDB or pure Lehman-Yao):**
- Pure InnoDB: requires SX-latch infrastructure (complex to implement in Rust)
- Pure Lehman-Yao: requires right-links + high-keys on every page (format change)
- Hybrid: InnoDB-style optimistic/pessimistic descent + Lehman-Yao right-link for readers

**Chosen protocol:**

| Operation | Descent | Leaf | Split handling |
|---|---|---|---|
| **Search (point lookup)** | S-latch coupling (release parent after child acquired) | S-latch on leaf | N/A |
| **Range scan** | S-latch to find start leaf, then follow next_leaf chain | S-latch per leaf | Move-right if concurrent split detected |
| **Insert (optimistic)** | S-latch coupling on internals | X-latch on leaf only | If leaf full → drop all latches, restart pessimistic |
| **Insert (pessimistic)** | X-latch coupling (hold parent while acquiring child) | X-latch on leaf | Split: hold current X + allocate new + X-latch parent |
| **Delete (optimistic)** | S-latch coupling | X-latch on leaf | If underfull → drop all, restart pessimistic |
| **Delete (pessimistic)** | X-latch coupling | X-latch on leaf | Merge: hold both siblings X + parent X |

## Latch coupling protocol (detailed)

### Read descent (search, range scan start)

```
acquire S-latch(root)
for each internal level:
    find child_idx via binary search
    acquire S-latch(child)
    release S-latch(parent)   ← coupling: never hold both long
at leaf:
    perform search under S-latch
    release S-latch(leaf)
```

**Invariant:** At most 2 S-latches held simultaneously (parent + child, briefly).

### Optimistic write descent (insert, delete — 95% of cases)

```
acquire S-latch(root)
for each internal level:
    find child_idx
    acquire S-latch(child)
    release S-latch(parent)
at leaf:
    upgrade S-latch → X-latch(leaf)  [or release S + acquire X]
    if operation succeeds (fits in page):
        modify leaf under X-latch
        release X-latch(leaf)
        return SUCCESS
    else (page full / underfull):
        release X-latch(leaf)
        restart with PESSIMISTIC descent
```

**Key insight**: 95%+ of inserts don't cause splits. Optimistic avoids X-latching
internal nodes in the common case → much less contention.

### Pessimistic write descent (split/merge needed — 5% of cases)

```
acquire X-latch(root)
for each internal level:
    find child_idx
    check: is child "safe"? (has room for insert / won't underflow on delete)
    if safe:
        release X-latch(parent)  ← early release: child won't propagate up
        acquire X-latch(child)
    else:
        acquire X-latch(child)
        KEEP X-latch(parent)     ← parent may need update
at leaf:
    already X-latched
    perform operation (may trigger split/merge)
    if split:
        allocate new page (X-latched by us since we just created it)
        redistribute data between old and new page
        insert separator into parent (parent still X-latched)
        if parent also splits → propagate up (parent's parent also X-latched)
    release all X-latches (bottom-up)
```

**"Safe" page definition:**
- For INSERT: page has enough space for the record + potential reorganization headroom
  (`free_space > record_size + BTR_CUR_PAGE_REORGANIZE_LIMIT`)
- For DELETE: page has enough entries that removing one won't trigger merge
  (`num_keys > MIN_KEYS + 1`)

**Early release optimization:** If a child page is "safe", the parent X-latch can be
released immediately because the operation won't propagate upward. This reduces the
contention window significantly.

### Range scan under concurrent splits

```
S-latch(leaf_1), read entries, follow next_leaf to leaf_2
acquire S-latch(leaf_2)
release S-latch(leaf_1)

If leaf_1 was split between our read and the next_leaf follow:
  → next_leaf pointer was already updated during split
  → we follow to the correct new page (right sibling)
  → no data loss (split moves upper half to new right page)
```

The `next_leaf` chain is updated atomically during split (both pages written under
X-latch before any latch release). A reader following the chain always sees a
consistent state.

## Latch ordering (deadlock prevention)

**Rule 1: Tree descent order** — always acquire child before releasing parent
(standard latch coupling, prevents missing data during concurrent split).

**Rule 2: Sibling order** — when latching siblings during split/merge:
LEFT sibling → current → RIGHT sibling (ascending page_id order).

**Rule 3: Never reverse** — if holding X-latch(page A), never acquire X-latch(page B)
where B < A in the tree descent path (would reverse the coupling order).

**Rule 4: PageLockTable integration** — B-tree page latches come from the same
PageLockTable (40.3) as heap page latches. Lock ordering between B-tree and heap:
always acquire B-tree latch first, then heap latch (if both needed in same operation).

## Data structure changes

### ParentStack (descent context)

```rust
/// Stack of parent page references during descent.
/// Used by pessimistic path to access parent for split propagation.
struct ParentStack {
    entries: Vec<ParentEntry>,
}

struct ParentEntry {
    page_id: u64,
    child_idx: usize,      // which child we descended into
    latch_held: bool,       // true if X-latch still held (pessimistic)
}
```

### LatchGuard integration

Page latches from PageLockTable (40.3) return RAII guards. The B-tree code must
hold these guards for the correct duration:
- S-guard dropped immediately after use (coupling)
- X-guard held through modification + write_page

### Both B-trees (index + clustered)

The latch coupling protocol applies to BOTH:
1. **Index B-tree** (tree.rs): `insert()`, `delete()`, `range_in()`, `lookup_in()`
2. **Clustered B-tree** (clustered_tree.rs): `insert()`, `delete_mark()`, `update_in_place()`, `range()`, `lookup()`

Same protocol, same latch ordering, same optimistic/pessimistic logic.

## Concurrency guarantees

| Scenario | Behavior | Mechanism |
|---|---|---|
| 2 concurrent searches (same key) | **Parallel** | Both use S-latch coupling, S+S compatible |
| Search during insert (different key) | **Parallel** | Search: S-latch. Insert optimistic: S-latch on internals, X only at leaf |
| Search during insert (same leaf) | **Brief serialization** | Search S-latch waits for insert X-latch release (~1µs) |
| 2 concurrent inserts (different leaves) | **Parallel** | Optimistic: X-latch only on different leaves |
| 2 concurrent inserts (same leaf, no split) | **Serialized** | X-latch on same leaf — second waits |
| 2 concurrent inserts (one causes split) | **Serialized at parent** | Pessimistic: X-latch from root. Other insert waits for root X-latch |
| Insert during range scan | **Mostly parallel** | Scan holds S-latch on current leaf only. Insert X-latches a different leaf |
| Delete causing merge + insert causing split | **Serialized** | Both need X-latch on parent. One waits |

## Use cases

1. **High-throughput INSERT (8 threads, unique keys):**
   95% of inserts are optimistic (single leaf X-latch). 8 threads hit 8 different leaves
   → full parallelism. 5% trigger splits → pessimistic restart → brief root X-latch.

2. **Mixed SELECT + INSERT:**
   SELECT uses S-latch coupling (readers never block). INSERT uses optimistic X-latch
   at leaf only. Readers and writers run concurrently on different tree levels.

3. **Range scan during bulk INSERT:**
   Scanner holds S-latch on one leaf at a time, follows next_leaf chain.
   Inserter X-latches a different leaf. No contention unless they hit the same leaf
   (briefly serialized, ~1µs).

4. **Concurrent DELETE + INSERT on same leaf:**
   Both acquire X-latch on the leaf. Second operation waits. First completes and releases.
   If DELETE causes underfull → pessimistic restart with X-latch coupling from root.

## Acceptance criteria

- [ ] S-latch coupling for read descent (search, range scan start)
- [ ] Optimistic write: S-latch descent + X-latch at leaf
- [ ] Pessimistic write: X-latch coupling with "safe page" early release
- [ ] Split under X-latch: current page + new page + parent all X-latched
- [ ] Merge under X-latch: both siblings + parent all X-latched
- [ ] ParentStack tracks descent path for pessimistic split propagation
- [ ] Latch ordering: parent before child, left before right sibling
- [ ] Range scan: S-latch per leaf, released before next leaf acquired
- [ ] "Safe page" check enables early parent latch release (reduces contention)
- [ ] Works for BOTH index B-tree (tree.rs) AND clustered B-tree (clustered_tree.rs)
- [ ] Concurrent insert to different leaves: verified parallel (timing test)
- [ ] Concurrent insert causing split: verified correct tree structure
- [ ] Concurrent search during insert: verified no missing/phantom keys
- [ ] Stress test: 8 threads × 10K inserts → tree structure valid, all keys reachable
- [ ] No deadlock under any operation combination (latch ordering verified)
- [ ] `cargo clippy -- -D warnings` clean

## Out of scope

- SX-latch (shared-exclusive intermediate mode) — too complex for first iteration.
  Use X-latch for pessimistic, S for optimistic. SX can be added as optimization later.
- Lehman-Yao incomplete split flag (lazy split completion) — requires page format change.
  Can be added later for reduced contention window.
- Lock escalation (row lock → page lock → table lock)
- B-tree node compression under concurrency

## Dependencies

- 40.3 (StorageEngine interior mutability) — PageLockTable provides per-page RwLocks
- 40.5 (Lock Manager) — row-level locks for the data protected by the index
- 40.7 (HeapChain concurrent) — heap pages use same PageLockTable
