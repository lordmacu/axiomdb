# Spec: cursor-reuse-cross-statement — last-touched-leaf hint

Phase: perf-sqlite-gap — close embedded gap with SQLite
Task: Attack 5 — cache the most-recently-touched clustered leaf on
`SessionContext`; skip the B-tree descent when the next key falls in
that leaf's range
Status: approved

## Context

The current clustered-tree lookup path
([`clustered_tree/mod.rs:478`](crates/axiomdb-storage/src/clustered_tree/mod.rs:478))
calls `lookup_physical` → `descend_to_leaf` from the root **on every
call**. For workloads where consecutive statements touch the same leaf
(autocommit AUTO_INCREMENT INSERT into the rightmost leaf, range
scans of overlapping ranges, point lookups within a hot key range),
this is wasted work.

SQLite's `BtCursor` carries `pCur->pPage` plus `BTCF_ValidNKey` cached
metadata and reuses them between successive `sqlite3BtreeInsert` /
`sqlite3BtreeMovetoUnpacked` calls — the fast path at
[`research/sqlite/src/btree.c:9482-9491`](research/sqlite/src/btree.c)
skips the descent entirely when the same page covers the next key.
Their comment at lines 9656-9663 explicitly calls out "a big
performance boost" for monotonically increasing keys.

We have a partial analog (`HeapAppendHint` in
[`crates/axiomdb-sql/src/session.rs:1099`](crates/axiomdb-sql/src/session.rs))
but it covers heap tables only and only the tail-append case. Attack
5 adds a clustered-leaf equivalent that works for any read or write,
not just appends.

## Goal

For consecutive statements that touch keys in the same clustered leaf,
skip the `descend_to_leaf` call by reading the cached leaf page
directly.

## Non-goals

- **Multi-leaf cache (LRU per table).** Approach B in the brainstorm.
  Deferred to a follow-up if profiling shows multi-table workloads
  thrashing the single slot.
- **Full SQLite-style `BtCursor` abstraction.** Approach C in the
  brainstorm. Too invasive for one task; revisit in v1.0.
- **Index B-tree cursor reuse** (non-PK indexes). Spec covers the
  clustered (primary) B-tree only. Secondary indexes are a separate
  follow-up.
- **Heap-table cursor reuse beyond the existing tail hint.** Out of
  scope; heap tables are not the bench bottleneck.
- **Cross-session cursor sharing.** Per-`SessionContext` only.
- **USESEEKRESULT-style cursor handoff between constraint check and
  insert.** That is Attack 7, a separate spec.

## Behavior

### Public API

No new public API on `axiomdb-embedded` or `axiomdb-sql`. Internal
additions:

```rust
// crates/axiomdb-sql/src/session.rs — new struct + 3 methods on SessionContext

/// A cached pointer to the last clustered-leaf page this session
/// touched. Used to skip the B-tree descent when consecutive lookups
/// fall in the same leaf's key range.
#[derive(Debug, Clone)]
pub struct LastClusteredLeaf {
    /// Owner table — different tables don't share the hint.
    pub table_id: u32,
    /// `TableDef.root_page_id` at the time the hint was recorded.
    /// Used to detect root rotation (e.g. bulk DELETE that frees the
    /// old root).
    pub root_page_id: u64,
    /// The leaf page itself.
    pub leaf_page_id: u64,
    /// Lowest key currently stored in the leaf (cell 0).
    pub min_key: Vec<u8>,
    /// Highest key currently stored in the leaf (cell N-1).
    pub max_key: Vec<u8>,
    /// `TableDef.schema_version` at hint time — invalidated on bump.
    pub schema_version: u64,
}

impl SessionContext {
    /// Returns the cached leaf hint if it can serve `key`.
    ///
    /// "Can serve" means:
    /// - `table_id` matches
    /// - `root_page_id` matches the caller's current root
    /// - `schema_version` matches the caller's current version
    /// - `min_key ≤ key ≤ max_key`
    ///
    /// Returns `None` on any mismatch (caller must descend from root).
    pub fn get_clustered_leaf_hint(
        &self,
        table_id: u32,
        root_page_id: u64,
        schema_version: u64,
        key: &[u8],
    ) -> Option<&LastClusteredLeaf>;

    /// Updates the hint after a descent or successful re-read.
    pub fn set_clustered_leaf_hint(&mut self, hint: LastClusteredLeaf);

    /// Invalidates the hint. Called when the caller mutates the leaf
    /// in a way that may have changed `min_key`/`max_key` (split,
    /// merge, first/last cell update).
    pub fn invalidate_clustered_leaf_hint(&mut self);

    /// Diagnostic / test accessor.
    pub fn clustered_leaf_hint_present(&self) -> bool;
}
```

The hint is also cleared by the existing `invalidate_all` (called on
DDL and after batch flushes), since either may change the leaf's
contents.

### Semantics

`clustered_tree::lookup_physical` new behavior, in the optional
session-context branch:

1. Read the cached `LastClusteredLeaf` hint via
   `SessionContext::get_clustered_leaf_hint(table_id, root_pid,
   schema_version, key)`.
2. On hit: read `leaf_page_id` directly, perform `leaf_search_checked`
   on the in-memory bytes, return the row (or `None`).
3. On miss: full descent via `descend_to_leaf`. Update the hint with
   the new leaf's pid + min/max keys at the end.

The lookup behaves identically — same returned `ClusteredRow`, same
errors, same MVCC visibility. The only observable difference is fewer
page reads on the hot path.

**Why is it safe** despite concurrent writes:
- `read_page` always returns the current bytes of the page (whatever
  another connection's commit wrote).
- A SPLIT moves SOME keys to a NEW page but the original page still
  holds keys in its new range. Our `min_key`/`max_key` may be stale,
  but the worst case is a hit-followed-by-not-found (key was moved to
  a sibling); the caller can detect this via `leaf_search_checked` and
  fall back to descent.
- A MERGE makes the page disappear; `read_page` returns invalid page
  type or zero cells — we detect and fall back.
- An UPDATE to the boundary cell extends or shrinks the range; our
  cached `min_key`/`max_key` may be slightly off but the caller's
  `leaf_search_checked` either finds the key or returns `Err(insert_pos)`
  — both interpretable.

**Invariant**: on a hit that fails `leaf_search_checked` for "out of
range" reasons (key < min_key or key > max_key actually-stored), the
hint is invalidated and the caller descends. This bounds false-positive
cost to one extra page read.

### Write-path semantics

For INSERT into a clustered table:
- The batched path (`flush_clustered_insert_batch` →
  `try_insert_rightmost_leaf_batch`) already operates on a known
  rightmost leaf; not affected.
- The autocommit / per-row insert path (used by `INSERT INTO t VALUES
  (...)` without an active txn) currently calls
  `clustered_tree::insert` which descends every time. Attack 5
  augments this path: if the hint's `max_key < new_key`, append
  directly to `hint.leaf_page_id` (calling the existing
  `try_insert_rightmost_leaf` or its equivalent in-place); otherwise
  fall back to descent.

This delivers SQLite's `OPFLAG_APPEND` win for AUTO_INCREMENT
INSERTs in autocommit mode without changing semantics.

### Error cases

| Input | Expected error | Note |
|-------|----------------|------|
| Cached leaf was freed (page type ≠ ClusteredLeaf) | Invalidate, descend. No user-visible error. | — |
| Cached leaf shrunk to 0 cells (merged away) | Invalidate, descend. | — |
| `min_key`/`max_key` differ from actual cells | Invalidate, descend. | False-positive: 1 extra page read. |
| schema_version bumped | Hint returns None automatically. | Pre-existing pattern (Attack 3.A). |
| root_page_id changed (rotation) | Hint returns None automatically. | — |

No new `DbError` variants.

### Cross-path impact

| Path | Expected speedup |
|------|------------------|
| autocommit clustered INSERT (AUTO_INC) | **5-10×** — all rows hit the same rightmost leaf |
| consecutive point lookups in same key range | 1.5-3× depending on locality |
| range_scan across overlapping ranges | ~2× |
| DELETE per row (crud_flow/delete) | ~2× |
| count_star (single full scan) | ~1.1× (mostly unchanged — scan iterator doesn't repeatedly descend) |
| insert_batch (already batched) | unchanged |
| group_by | unchanged |

## Edge cases

Each becomes a test case in the plan:

- [ ] Hit: consecutive lookups of keys in the same leaf return correct
  rows; hint stays populated.
- [ ] Miss by key range: lookup of a key outside the cached leaf
  descends from root.
- [ ] Miss by table_id: lookup on a different table descends and
  updates the hint.
- [ ] Miss by schema_version: ALTER TABLE bumps version, next lookup
  descends.
- [ ] Miss by root_page_id: bulk DELETE rotates the root, next lookup
  descends.
- [ ] Stale hint after concurrent split (another conn): hit
  successfully reads the page but `leaf_search_checked` returns
  out-of-range → fall back to descent.
- [ ] Hint cleared by DDL via `invalidate_all`.
- [ ] Hint cleared by batch flush.
- [ ] Autocommit INSERT into rightmost leaf reuses the hint (no
  descent on the 2nd+ row).
- [ ] Autocommit INSERT of a non-monotonic key falls back to descent
  correctly.
- [ ] Empty table lookup (no hint possible since no leaf cells yet).

## On-disk format

No on-disk format change. Hint is purely in-memory per session.

## Performance budget

Baseline (post-Attack-3.B, pre-Attack-5):

| Metric | Today | Target after Attack 5 |
|--------|------:|----------------------:|
| `insert_autocommit` throughput | 8.7K rows/s | **≥ 50K rows/s** (5-10×) |
| `point_lookup` throughput | 8.9K ops/s | **≥ 13K ops/s** (~1.5×) |
| `range_scan` throughput | 727K rows/s | **≥ 1.2M rows/s** (~1.7×) |
| `crud_flow/delete` throughput | 1.09M rows/s | **≥ 2M rows/s** (~2×) |
| `insert_batch` throughput | 20.7K rows/s | unchanged |
| `full_scan`, `select_where`, `group_by` | (current) | unchanged |
| Workspace test runtime | baseline | within +5% |

Measured via `axiomdb_bench --compare --rows 10000` (3 runs, take
median).

## Dependencies

- Depends on:
  - `TableDef.schema_version` infra (Attack 3.A — already landed).
  - `clustered_tree::lookup_physical` and `descend_to_leaf` (already
    present).
  - The existing `try_insert_rightmost_leaf` /
    `try_insert_rightmost_leaf_batch` (already present, used by Attack
    5's write path).
- Blocks:
  - Attack 7 (USESEEKRESULT) — that work can layer on top of the same
    hint slot.
  - Attack 6 (full-blown cursor) — Attack 5 is the single-slot
    precursor; if profiling justifies it later, upgrade to multi-slot.

## Open questions

All resolved during brainstorm. Nothing pending.

## Done criteria

- [ ] `axiomdb_bench --compare --rows 10000` shows
  `insert_autocommit ≥ 50K rows/s` (≥ 5× baseline 8.7K).
- [ ] `axiomdb_bench --compare --rows 10000` shows
  `point_lookup ≥ 13K ops/s` (≥ 1.5× baseline 8.9K).
- [ ] `axiomdb_bench --compare --rows 10000` shows no regression on
  `insert_batch`, `full_scan`, `select_where`, `group_by` (within
  ±5%).
- [ ] `cargo nextest run --workspace` (Lima) — clean.
- [ ] `cargo clippy --workspace -- -D warnings` (Lima) — clean.
- [ ] `cargo fmt --check` — clean.
- [ ] `tools/wire-test.py` — clean (per memory pre-flight rule).
- [ ] New test module
  `crates/axiomdb-sql/tests/integration_cursor_reuse.rs` with at
  least one test per edge-case bullet above.
- [ ] Rustdoc on every new public item.
- [ ] `SessionContext::clustered_leaf_hint_present()` is a `pub` test
  accessor.

## References

External:
- SQLite cursor fast path: [`research/sqlite/src/btree.c:9482-9491`](research/sqlite/src/btree.c)
- SQLite multi-insert leaf-reuse rationale: [`research/sqlite/src/btree.c:9656-9663`](research/sqlite/src/btree.c)
- SQLite OPFLAG_APPEND: [`research/sqlite/src/insert.c:1516,1540`](research/sqlite/src/insert.c)
- Deep review: [`docs/sqlite-insert-deep-review.md`](docs/sqlite-insert-deep-review.md)

Internal:
- `clustered_tree::lookup_physical`: [`crates/axiomdb-storage/src/clustered_tree/mod.rs:494`](crates/axiomdb-storage/src/clustered_tree/mod.rs)
- `clustered_tree::descend_to_leaf`: [`crates/axiomdb-storage/src/clustered_tree/mod.rs:520`](crates/axiomdb-storage/src/clustered_tree/mod.rs)
- `try_insert_rightmost_leaf_batch`: [`crates/axiomdb-storage/src/clustered_tree/mod.rs:361`](crates/axiomdb-storage/src/clustered_tree/mod.rs)
- Existing `HeapAppendHint`: [`crates/axiomdb-sql/src/session.rs:1099`](crates/axiomdb-sql/src/session.rs)
- Existing `schema_version` cache pattern (Attack 3.A): [`spec-insert-setup-dedup-A.md`](specs/fase-perf-sqlite-gap/spec-insert-setup-dedup-A.md)
- Brainstorm (this conversation, 2026-05-17)
