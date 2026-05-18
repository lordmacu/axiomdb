# Plan: clustered-batch-defer

Phase: perf-sqlite-gap
Task: Attack 10 v1 — defer secondary + catalog batching + re-arm hint
Spec: specs/fase-perf-sqlite-gap/spec-clustered-batch-defer.md
Status: done

## Summary

The 3 optimizations are independent. Land them one at a time, measure
between each to know which one moved the needle. Order picked by
expected-impact and ease-of-rollback:

1. Defer secondary index maintenance (biggest expected win,
   localized refactor of `apply_clustered_insert_rows`)
2. Defer catalog persistence (smaller refactor on top of #1)
3. Re-arm rightmost-leaf hint (smallest change, biggest risk because
   it's in storage layer)
4. Measure on bench, document
5. Closing

## Dependencies

- [x] spec approved
- [x] Attack 7 v1.1 + Attack 8 landed

Blocks:
- Attack 11 (WAL batching, bulk-leaf construction) — depends on
  knowing whether Attack 10 closed enough of the gap

## Affected files

- `crates/axiomdb-sql/src/executor/insert_clustered.rs` —
  `apply_clustered_insert_rows` refactored
- `crates/axiomdb-storage/src/clustered_tree/mod.rs` — possibly add
  a helper to expose "what's the new rightmost leaf after the
  previous filled" (Step 3)
- `crates/axiomdb-sql/tests/integration_*.rs` — new test for
  clustered + secondary index + large batch
- `benches/comparison/axiomdb_bench/src/main.rs` — re-measure
  insert_appender after each step (no code change needed)
- `docs/perf-sqlite-gap.md` — Attack 10 subsection
- `memory/project_sqlite_baseline.md` — Attack 10 entry

---

## Step 1 — Defer secondary index maintenance

**Goal:** In `apply_clustered_insert_rows`, separate the primary
insert from the secondary insert. Pass 1 does only primary +
captures all secondary entries (key+rid pairs) in memory. Pass 2
loops over the captured entries and inserts into each secondary
B-Tree.

**Files:** `insert_clustered.rs`
**Approach:** TDD — first add an integration test for a 1000-row
clustered batch with two secondary indexes (point-query each row
via each index to verify visibility). The existing test
`appender_supports_table_with_multiple_indexes` covers this with
50 rows; extend or add a new test at 1000.

### Test to add

```rust
#[test]
fn appender_clustered_secondary_indexes_1000_rows() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT PRIMARY KEY, age INT, name TEXT)").unwrap();
    db.run("CREATE INDEX idx_age ON t (age)").unwrap();
    db.run("CREATE INDEX idx_name ON t (name)").unwrap();
    let mut app = db.appender("t").unwrap();
    for i in 1..=1000i32 {
        app.append_int(i).unwrap();
        app.append_int(20 + i % 50).unwrap();
        app.append_text(&format!("user_{i:04}")).unwrap();
        app.end_row().unwrap();
    }
    app.finish().unwrap();
    // Verify via secondary indexes:
    let by_age = db.query("SELECT COUNT(*) FROM t WHERE age = 25").unwrap();
    assert!(matches!(by_age[0][0], Value::BigInt(n) if n > 0));
    let by_name = db.query("SELECT id FROM t WHERE name = 'user_0500'").unwrap();
    assert_eq!(by_name.len(), 1);
    assert_eq!(by_name[0][0], Value::Int(500));
}
```

### Implementation outline

Inside `apply_clustered_insert_rows`, the existing per-row loop
already does:
1. Primary insert (currently in slow or fast path)
2. WAL append
3. Secondary index insert per index (the costly part)
4. Catalog persist on root change (also costly — Step 2)

After Step 1 the structure is:

```rust
// Pass 1: primary + WAL + capture secondary entries
let mut deferred_secondary: Vec<Vec<(Vec<u8>, RecordId)>> = 
    vec![Vec::with_capacity(rows.len()); secondary_indexes.len()];

for row in &rows {
    // ... existing primary + WAL ...
    // Replace per-row secondary insert with:
    for (idx_pos, secondary) in secondary_indexes.iter().enumerate() {
        let layout = &secondary_layouts[idx_pos];
        let entry = layout.entry_from_row(row, /*old_rid=*/None, &compiled_preds[idx_pos])?;
        if let Some(e) = entry {
            deferred_secondary[idx_pos].push((e.encoded_key, e.rid));
        }
    }
}

// Pass 2: bulk-insert into each secondary B-Tree
for (idx_pos, entries) in deferred_secondary.into_iter().enumerate() {
    let idx = &mut secondary_indexes[idx_pos];
    let root_pid = std::sync::atomic::AtomicU64::new(idx.root_page_id);
    for (key, rid) in entries {
        BTree::insert_in(storage, &root_pid, &key, rid, idx.fillfactor)?;
        if let Some(ct) = conn_txn.as_mut() {
            txn.record_index_insert(ct, idx.index_id, root_pid.load(...), key);
        }
    }
    let new_root = root_pid.load(...);
    if new_root != idx.root_page_id {
        // Catalog persist — Step 1 still does it per index per
        // change-detected; Step 2 will batch further.
        CatalogWriter::new(...)?.update_index_root(idx.index_id, new_root)?;
        idx.root_page_id = new_root;
    }
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-embedded --test integration_appender
./tools/vm.sh test -p axiomdb-sql  # SQL INSERT path uses the same function
limactl shell axiomdb -- /home/cristian.guest/axiomdb-target/release/axiomdb_bench --scenario insert_appender --rows 5000
# Compare ops/s vs baseline ~82K
```

### Commit

`feat(perf-sqlite-gap): Attack 10 step 1 — defer clustered secondary index inserts`

---

## Step 2 — Defer catalog persistence

**Goal:** Move `CatalogWriter::update_table_root` and
`update_index_root` from inline-when-root-changes to a single call
at the end of `apply_clustered_insert_rows`. Track root changes via
local variables.

### Implementation

Add tracking:

```rust
let mut new_primary_root: Option<u64> = None;
let mut deferred_index_roots: HashMap<IndexId, u64> = HashMap::new();
```

Update both whenever a root changes (in pass 1 for primary, pass 2
for secondary). At the end:

```rust
let mut writer = CatalogWriter::new(storage, txn, conn_txn)?;
if let Some(root) = new_primary_root {
    writer.update_table_root(table_def.id, root)?;
}
for (index_id, root) in deferred_index_roots {
    writer.update_index_root(index_id, root)?;
}
```

### Tests

The Step 1 test suffices. Add one for "row triggers multiple splits
in the primary tree" — verify final root is correct and persisted.

```rust
#[test]
fn appender_clustered_many_splits_catalog_root_correct() {
    let (_dir, mut db) = open_db();
    // Use TEXT PK with random-order keys to force splits.
    db.run("CREATE TABLE t (id TEXT PRIMARY KEY, v INT)").unwrap();
    let mut app = db.appender("t").unwrap();
    // Reverse-sorted insertion forces left-side splits.
    for i in (0..2000).rev() {
        app.append_text(&format!("key_{i:08}")).unwrap();
        app.append_int(i).unwrap();
        app.end_row().unwrap();
    }
    app.finish().unwrap();
    // All rows must be visible.
    assert_eq!(
        db.query("SELECT COUNT(*) FROM t").unwrap()[0][0],
        Value::BigInt(2000)
    );
    // Random spot-check: a row in the middle.
    let rows = db.query("SELECT v FROM t WHERE id = 'key_00001000'").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Int(1000));
}
```

### Verification + bench

Same as Step 1 — rerun bench, expect another bump.

### Commit

`feat(perf-sqlite-gap): Attack 10 step 2 — batch catalog persistence`

---

## Step 3 — Re-arm rightmost-leaf hint

**Goal:** In `apply_clustered_insert_rows`, after
`try_insert_rightmost_leaf_batch` returns partial (i.e. a leaf
filled), advance the hint to the new rightmost leaf and try again
on the remaining rows. Today the existing fast path is one-shot.

### Approach

Look at how the slow path (`insert_with_batch`) advances internal
state. The leaf that fills is replaced by 2 children; the rightmost
is the new "rightmost leaf". We need to either:
- Re-query the tree for the rightmost leaf after each fill (read
  cost — defeats the purpose)
- OR have `try_insert_rightmost_leaf_batch` return the new
  rightmost leaf page id

The cleanest is option (b): modify the storage helper to also
return the new rightmost leaf id (or None if the tree is now empty
of rightmost-leaf hint info).

### Test

```rust
#[test]
fn appender_clustered_sorted_50k_uses_fast_path() {
    // Functional test — just verify correctness; perf is in the bench.
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
    let mut app = db.appender("t").unwrap();
    for i in 0..50_000i32 {
        app.append_int(i).unwrap();
        app.append_int(i * 2).unwrap();
        app.end_row().unwrap();
    }
    app.finish().unwrap();
    assert_eq!(
        db.query("SELECT COUNT(*) FROM t").unwrap()[0][0],
        Value::BigInt(50_000)
    );
    // Spot-check ordering.
    let last = db.query("SELECT v FROM t WHERE id = 49999").unwrap();
    assert_eq!(last[0][0], Value::Int(49_998 * 2));
}
```

### Risk

This step touches `crates/axiomdb-storage/src/clustered_tree/mod.rs`.
If the storage layer's invariants are subtle, the change could break
something. If implementation reveals the function signature change is
intrusive, fall back to "re-query for rightmost leaf after each
fill" — slower but simpler.

### Commit

`feat(perf-sqlite-gap): Attack 10 step 3 — re-arm rightmost leaf hint after fill`

---

## Step 4 — Bench + verify

**Goal:** Measure the cumulative gain on Lima. Document.

### Run

```bash
limactl shell axiomdb -- bash -c "
  cd ~/axiomdb-target/release
  rm -rf /tmp/axiomdb_bench/*
  for i in 1 2 3; do
    rm -rf /tmp/axiomdb_bench/*
    ./axiomdb_bench --scenario insert_appender --rows 5000
  done
  for i in 1 2 3; do
    rm -rf /tmp/axiomdb_bench/*
    ./axiomdb_bench --scenario insert_appender_heap --rows 5000
  done
"
```

Capture the three iterations; report the median.

### Done criteria check

- ≥ 130K ops/s on clustered → success per spec budget
- 100-129K → partial win; document, decide whether Attack 11
  (WAL batching) or bulk-leaf is next
- < 100K → unexpected; profile to find the real bottleneck

### Commit (if no code changes needed)

`docs(perf-sqlite-gap): Attack 10 step 4 — bench result captured`

---

## Step 5 — Closing

- workspace nextest
- clippy on touched
- fmt
- docs/perf-sqlite-gap.md: Attack 10 subsection
- docs-site: no change (no API surface change)
- memory/project_sqlite_baseline.md update
- spec → implemented, plan → done
- Final commit

`feat(perf-sqlite-gap): Attack 10 step 5 — close clustered batch-defer`

---

## Risk register

| Risk | Likelihood | Mitigation |
|---|---|---|
| Per-row secondary insert in pass 2 is just as slow as before — gain mostly comes from catalog batching | Medium | Step-by-step measurement reveals this; if so, the v1 win comes from Step 2, not Step 1. Either way we learn. |
| Deferred secondary insert breaks the "appender + secondary index UNIQUE violation" semantics (now the violation fires at end of flush, not at the offending row) | Low | Same txn semantics — rollback aborts everything either way. The error text might be less precise about which row triggered it; acceptable for v1 |
| Re-arming the hint exposes a hidden invariant in the storage layer (e.g. the leaf chain must be coherent before next call) | Medium | Step 3 is incremental and reversible; if tests fail we revert that step |
| Pass 1's primary insert still calls `update_table_root` inside `insert_with_batch` (i.e. the per-row catalog cost is inside the storage layer, not in apply_clustered_insert_rows) | Medium | Verify with a printout; if so, the storage-layer call must be deferred too |
| The heap path uses the same helper but doesn't have the same issue — no regression there | Low | Heap path doesn't call `apply_clustered_insert_rows` (different code path); only clustered is affected |

## Rollback plan

If abandoned:

1. `git reset --hard <commit before Step 1>` — revert all 3 steps
2. The v1.1 + A8 surface is untouched
3. Spec status → blocked

## Estimated effort

Total: **2-3 days** (Step 1: 0.5d, Step 2: 0.5d, Step 3: 1d (storage
layer is the risky part), Step 4: 0.5h, Step 5: 0.5d)
