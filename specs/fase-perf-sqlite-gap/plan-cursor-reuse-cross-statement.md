# Plan: cursor-reuse-cross-statement

Phase: perf-sqlite-gap — close embedded gap with SQLite
Task: Attack 5 — last-touched clustered leaf hint
Spec: specs/fase-perf-sqlite-gap/spec-cursor-reuse-cross-statement.md
Status: done (cursor reuse correctness verified at 100% hit rate;
bench perf budget NOT met due to orthogonal bottlenecks documented
in docs/perf-sqlite-gap.md "Attack 5" section)

## Summary

Five TDD-ordered steps. **5.1** adds `LeafCursorHint` struct +
`lookup_with_hint()` helper in `axiomdb-storage` — pure data + a
hint-aware lookup. **5.2** adds the storage slot + 3 methods on
`SessionContext` in `axiomdb-sql` — connects sql-side state to the
storage-side helper. **5.3** switches the SELECT/UPDATE/DELETE call
sites to use the hint, plus invalidation hooks where needed. **5.4**
wires the **append fast-path for autocommit clustered INSERT** —
the biggest projected win (5-10×). **5.5** closes with measurements
against the spec budget, workspace gates, wire smoke, and docs.

Order picks low-risk infrastructure first (5.1, 5.2 don't change any
production behavior — only add unused API surface). Step 5.3 is the
first behavior change. Step 5.4 is the bench-mover. Each step has its
own commit, individually revertible.

## Dependencies

Must be done first:
- [x] spec-cursor-reuse-cross-statement approved (commit `a43ab17b`)
- [x] `TableDef.schema_version` infra (Attack 3.A — already landed)
- [x] `clustered_tree::descend_to_leaf` exists
- [x] `try_insert_rightmost_leaf` / `_batch` already present

Blocks (until this plan is done):
- Attack 7 (USESEEKRESULT) — layers on the same hint slot.
- Attack 6 (multi-slot LRU upgrade) — needs profiling data from 5
  first.

## Affected files

New files:
- `crates/axiomdb-sql/tests/integration_cursor_reuse.rs` — 11 spec
  edge-case tests + 2 perf-smoke tests

Modified files:
- `crates/axiomdb-storage/src/clustered_tree/mod.rs` — add
  `pub struct LeafCursorHint`, `pub fn lookup_with_hint`,
  `pub fn try_append_with_hint`
- `crates/axiomdb-sql/src/session.rs` — add storage slot
  (`clustered_leaf_hint: Option<LeafCursorHint>`) + 3 methods +
  clear in `invalidate_all` + clear in `invalidate_table`
- `crates/axiomdb-sql/src/table.rs` — switch the two
  `clustered_tree::lookup` call sites (lines ~382, ~516) to pass the
  hint
- `crates/axiomdb-sql/src/executor/delete.rs` — switch the two
  `clustered_tree::lookup` sites (lines ~474, ~618) to pass the hint
- `crates/axiomdb-sql/src/executor/update_clustered_helpers.rs` —
  switch lookup site (line ~44)
- `crates/axiomdb-sql/src/executor/insert_clustered.rs` — wire the
  append-hint path for the autocommit branch (around line ~335)
- `docs/perf-sqlite-gap.md` — Step 5.5 update with new numbers
- `memory/project_sqlite_baseline.md` — Step 5.5 update

---

## Step 5.1 — `LeafCursorHint` + `lookup_with_hint()` in storage

**Goal:** A pure-data hint struct + a hint-aware `lookup_physical`
variant in `axiomdb-storage`. No SQL-layer dependency.

**Files:**
- `crates/axiomdb-storage/src/clustered_tree/mod.rs` — add struct +
  helper
- `crates/axiomdb-storage/src/clustered_tree/tests_lookup.rs` — 4
  unit tests on the helper

**Approach:** TDD — write the helper's tests, then the helper.

### Tests to add

```rust
// In tests_lookup.rs

#[test]
fn lookup_with_hint_misses_when_empty() {
    // No hint → must descend; result identical to lookup_physical.
    let (storage, root) = setup_table_with_rows(&[1, 5, 10]);
    let mut hint = None;
    let row = lookup_with_hint(&storage, Some(root), &enc_key(5), &mut hint)
        .unwrap();
    assert!(row.is_some());
    // Hint populated after the descent.
    assert!(hint.is_some());
    let h = hint.as_ref().unwrap();
    assert_eq!(h.table_id, /* whatever id the helper uses; pass 42 in */ 42);
    assert_eq!(h.root_page_id, root);
}

#[test]
fn lookup_with_hint_hits_when_key_in_range() {
    let (storage, root) = setup_table_with_rows(&[1, 5, 10, 15, 20]);
    let mut hint = None;
    // First call populates.
    let _ = lookup_with_hint(&storage, Some(root), &enc_key(10), &mut hint)
        .unwrap();
    let leaf_after_first = hint.as_ref().unwrap().leaf_page_id;
    // Second call with key in same leaf — must REUSE.
    let _ = lookup_with_hint(&storage, Some(root), &enc_key(15), &mut hint)
        .unwrap();
    assert_eq!(
        hint.as_ref().unwrap().leaf_page_id,
        leaf_after_first,
        "same leaf reused"
    );
}

#[test]
fn lookup_with_hint_misses_on_key_out_of_range() {
    let (storage, root) = setup_table_with_rows_split_into_two_leaves();
    let mut hint = None;
    let _ = lookup_with_hint(&storage, Some(root), &enc_key(/* first leaf */ 1), &mut hint)
        .unwrap();
    let first_leaf = hint.as_ref().unwrap().leaf_page_id;
    // Key in OTHER leaf — must descend and update hint.
    let _ = lookup_with_hint(&storage, Some(root), &enc_key(/* second leaf */ 1000), &mut hint)
        .unwrap();
    assert_ne!(hint.as_ref().unwrap().leaf_page_id, first_leaf);
}

#[test]
fn lookup_with_hint_invalidates_on_root_mismatch() {
    let (storage, root) = setup_table_with_rows(&[1, 5, 10]);
    let mut hint = Some(LeafCursorHint {
        table_id: 42,
        root_page_id: 9999, // wrong root
        leaf_page_id: 1,
        min_key: vec![0],
        max_key: vec![255],
        schema_version: 1,
    });
    let _ = lookup_with_hint(&storage, Some(root), &enc_key(5), &mut hint)
        .unwrap();
    // Stale hint replaced with fresh one matching real root.
    assert_eq!(hint.as_ref().unwrap().root_page_id, root);
}
```

(Helper `setup_table_with_rows` already exists in `tests.rs`; reuse it.)

### Implementation outline

```rust
// crates/axiomdb-storage/src/clustered_tree/mod.rs

/// A per-session cached pointer to the most-recently-touched clustered
/// leaf page. Used by `lookup_with_hint` and `try_append_with_hint`
/// to skip the B-tree descent when the next key falls in the cached
/// leaf's range.
///
/// Storage-layer struct (no SQL dependency); the SQL layer owns the
/// `Option<LeafCursorHint>` slot inside its `SessionContext`.
#[derive(Debug, Clone)]
pub struct LeafCursorHint {
    pub table_id: u32,
    pub root_page_id: u64,
    pub leaf_page_id: u64,
    pub min_key: Vec<u8>,
    pub max_key: Vec<u8>,
    pub schema_version: u64,
}

/// Variant of `lookup_physical` that consults `hint` first.
///
/// Behavior:
/// - On hit (table_id + root + key range all match): re-reads the
///   cached `leaf_page_id`, performs `leaf_search_checked`, returns
///   the row.
/// - On in-range hit but `leaf_search_checked` returns out-of-range
///   (concurrent split moved the key): falls back to descent.
/// - On miss (any mismatch): descends from root; updates `hint` with
///   the new leaf's pid + min/max keys.
///
/// Returns the same `Result<Option<ClusteredRow>, DbError>` as
/// `lookup_physical`. Visibility is not checked (caller handles MVCC).
pub fn lookup_with_hint(
    storage: &dyn StorageEngine,
    root_pid: Option<u64>,
    key: &[u8],
    table_id: u32,
    schema_version: u64,
    hint: &mut Option<LeafCursorHint>,
) -> Result<Option<ClusteredRow>, DbError> {
    let Some(root_pid) = root_pid else {
        return Ok(None);
    };

    // ── Fast path: try the hint ──
    if let Some(h) = hint.as_ref() {
        if h.table_id == table_id
            && h.root_page_id == root_pid
            && h.schema_version == schema_version
            && key >= h.min_key.as_slice()
            && key <= h.max_key.as_slice()
        {
            // Re-read the page (bytes may have changed due to commits).
            let leaf = storage.read_page(h.leaf_page_id)?;
            if clustered_page_type(&leaf)? == PageType::ClusteredLeaf
                && clustered_leaf::num_cells(leaf.as_page()) > 0
            {
                if let Ok(pos) = leaf_search_checked(leaf.as_page(), key) {
                    return match pos {
                        Ok(pos) => {
                            let cell = clustered_leaf::read_cell(leaf.as_page(), pos as u16)?;
                            let row_data = reconstruct_row_data(storage, &cell)?;
                            Ok(Some(ClusteredRow {
                                key: cell.key.to_vec(),
                                row_header: cell.row_header,
                                row_data,
                            }))
                        }
                        Err(_) => Ok(None),
                    };
                }
                // Out-of-range despite passing the range check — split
                // happened concurrently. Fall through to descent.
            }
            // Leaf no longer valid — fall through and re-descend.
        }
    }

    // ── Slow path: descend from root ──
    let leaf = descend_to_leaf(storage, root_pid, key)?;

    // Record the leaf in the hint regardless of whether the lookup
    // finds the row.
    let nc = clustered_leaf::num_cells(leaf.as_page());
    if nc > 0 {
        let first = clustered_leaf::read_cell(leaf.as_page(), 0)?;
        let last = clustered_leaf::read_cell(leaf.as_page(), nc - 1)?;
        *hint = Some(LeafCursorHint {
            table_id,
            root_page_id: root_pid,
            leaf_page_id: leaf.page_id(),
            min_key: first.key.to_vec(),
            max_key: last.key.to_vec(),
            schema_version,
        });
    }

    let pos = match leaf_search_checked(leaf.as_page(), key)? {
        Ok(pos) => pos,
        Err(_) => return Ok(None),
    };
    let cell = clustered_leaf::read_cell(leaf.as_page(), pos as u16)?;
    let row_data = reconstruct_row_data(storage, &cell)?;
    Ok(Some(ClusteredRow {
        key: cell.key.to_vec(),
        row_header: cell.row_header,
        row_data,
    }))
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-storage --test clustered_tree -- --ignored=false
./tools/vm.sh clippy axiomdb-storage 2>&1 | tail -5
```

### Commit

```
feat(perf-sqlite-gap): step 5.1 — LeafCursorHint + lookup_with_hint

New storage-layer struct + helper for the cursor-reuse cross-statement
work. Adds:
  pub struct LeafCursorHint
  pub fn lookup_with_hint(...)

Hint-aware lookup that re-reads the cached leaf page when the key
falls in its range, falling back to descend_to_leaf on any
mismatch (different table, different root, schema_version bump,
key out of range, page no longer valid).

4 unit tests: miss-on-empty (populates), hit-in-range, miss-on-out-of-
range (re-descends), invalidates-on-root-mismatch.

Mirrors SQLite's BTCF_ValidNKey fast path at
research/sqlite/src/btree.c:9482-9491.

No call sites use it yet — pure addition; behavior unchanged.
```

---

## Step 5.2 — `SessionContext` storage slot + 3 methods

**Goal:** Add the slot to `SessionContext` and expose the get/set/
invalidate API.

**Files:**
- `crates/axiomdb-sql/src/session.rs` — add field + methods
- `crates/axiomdb-sql/tests/integration_cursor_reuse.rs` (new) — 3
  unit tests of the API

**Approach:** TDD red → green.

### Tests to add

```rust
// crates/axiomdb-sql/tests/integration_cursor_reuse.rs
use axiomdb_sql::SessionContext;
use axiomdb_storage::clustered_tree::LeafCursorHint;

fn fake_hint(table_id: u32, root: u64, leaf: u64, version: u64) -> LeafCursorHint {
    LeafCursorHint {
        table_id,
        root_page_id: root,
        leaf_page_id: leaf,
        min_key: vec![0],
        max_key: vec![255],
        schema_version: version,
    }
}

#[test]
fn leaf_hint_starts_absent() {
    let ctx = SessionContext::default();
    assert!(!ctx.clustered_leaf_hint_present());
}

#[test]
fn leaf_hint_set_then_get() {
    let mut ctx = SessionContext::default();
    ctx.set_clustered_leaf_hint(fake_hint(1, 100, 200, 1));
    assert!(ctx.clustered_leaf_hint_present());
    let h = ctx
        .get_clustered_leaf_hint(1, 100, 1, &[10u8])
        .expect("matching hint");
    assert_eq!(h.leaf_page_id, 200);
}

#[test]
fn leaf_hint_get_returns_none_on_mismatch() {
    let mut ctx = SessionContext::default();
    ctx.set_clustered_leaf_hint(fake_hint(1, 100, 200, 1));
    // Different table_id.
    assert!(ctx.get_clustered_leaf_hint(2, 100, 1, &[10u8]).is_none());
    // Different root.
    assert!(ctx.get_clustered_leaf_hint(1, 999, 1, &[10u8]).is_none());
    // Different schema_version.
    assert!(ctx.get_clustered_leaf_hint(1, 100, 2, &[10u8]).is_none());
}

#[test]
fn leaf_hint_invalidate_clears() {
    let mut ctx = SessionContext::default();
    ctx.set_clustered_leaf_hint(fake_hint(1, 100, 200, 1));
    ctx.invalidate_clustered_leaf_hint();
    assert!(!ctx.clustered_leaf_hint_present());
}
```

### Implementation outline

```rust
// crates/axiomdb-sql/src/session.rs

pub struct SessionContext {
    // ... existing fields ...

    /// Attack 5 (cursor-reuse-cross-statement): cached pointer to the
    /// most-recently-touched clustered leaf in this session. Used to
    /// skip the B-tree descent when the next key falls in this leaf's
    /// range. Cleared by `invalidate_all` and `invalidate_table`.
    clustered_leaf_hint: Option<axiomdb_storage::clustered_tree::LeafCursorHint>,
}

impl SessionContext {
    // ... existing methods ...

    pub fn get_clustered_leaf_hint(
        &self,
        table_id: u32,
        root_page_id: u64,
        schema_version: u64,
        key: &[u8],
    ) -> Option<&axiomdb_storage::clustered_tree::LeafCursorHint> {
        let h = self.clustered_leaf_hint.as_ref()?;
        if h.table_id == table_id
            && h.root_page_id == root_page_id
            && h.schema_version == schema_version
            && key >= h.min_key.as_slice()
            && key <= h.max_key.as_slice()
        {
            Some(h)
        } else {
            None
        }
    }

    pub fn set_clustered_leaf_hint(
        &mut self,
        hint: axiomdb_storage::clustered_tree::LeafCursorHint,
    ) {
        self.clustered_leaf_hint = Some(hint);
    }

    pub fn invalidate_clustered_leaf_hint(&mut self) {
        self.clustered_leaf_hint = None;
    }

    pub fn clustered_leaf_hint_present(&self) -> bool {
        self.clustered_leaf_hint.is_some()
    }

    /// Returns `&mut Option<...>` so callers can hand it to
    /// `lookup_with_hint` for in-place update.
    pub fn clustered_leaf_hint_slot(
        &mut self,
    ) -> &mut Option<axiomdb_storage::clustered_tree::LeafCursorHint> {
        &mut self.clustered_leaf_hint
    }
}
```

Also update `invalidate_all` and `invalidate_table` to clear the
slot:

```rust
pub fn invalidate_all(&mut self) {
    self.cache.clear();
    self.heap_tail.clear();
    self.clustered_leaf_hint = None;  // ← new
    self.holiday_cache.clear();
    self.exchange_rate_cache.clear();
}

pub fn invalidate_table(&mut self, ...) {
    // existing logic ...
    self.clustered_leaf_hint = None;  // ← new (could be more precise
                                       // but conservative is fine)
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql --test integration_cursor_reuse
./tools/vm.sh test -p axiomdb-sql       # broad safety net
./tools/vm.sh clippy axiomdb-sql 2>&1 | tail -5
```

### Commit

```
feat(perf-sqlite-gap): step 5.2 — SessionContext clustered_leaf_hint slot

Adds storage slot + 4 methods on SessionContext:
  get_clustered_leaf_hint(table_id, root, version, key) → Option<&Hint>
  set_clustered_leaf_hint(hint)
  invalidate_clustered_leaf_hint()
  clustered_leaf_hint_present() — diagnostic
  clustered_leaf_hint_slot() → &mut Option<Hint>  (for in-place update)

Also clears the slot from invalidate_all + invalidate_table.

4 unit tests cover starts-absent, set+get, mismatch-cases, invalidate.

No callers wired yet — pure addition; behavior unchanged.
```

---

## Step 5.3 — Switch read-path call sites (SELECT/UPDATE/DELETE)

**Goal:** The six existing `clustered_tree::lookup` / `lookup_physical`
call sites use `lookup_with_hint` and the SessionContext slot. SELECT
queries with `WHERE id = literal` start hitting the hint on the 2nd
call.

**Files:**
- `crates/axiomdb-sql/src/table.rs` (lines ~382, ~516)
- `crates/axiomdb-sql/src/executor/delete.rs` (lines ~474, ~618)
- `crates/axiomdb-sql/src/executor/update_clustered_helpers.rs` (line ~44)
- `crates/axiomdb-sql/src/fk_enforcement.rs` (line ~303)
- `crates/axiomdb-sql/src/clustered_secondary.rs` (line ~447)
- `crates/axiomdb-sql/tests/integration_cursor_reuse.rs` — 3
  end-to-end tests

**Approach:** TDD — write end-to-end tests that prove the hint is
populated and reused; then thread the hint through each call site.

### Tests to add

```rust
// crates/axiomdb-sql/tests/integration_cursor_reuse.rs (appended)

mod harness {
    // Same shape as integration_resolve_table_cache::harness — Memory
    // storage + TxnManager + SessionContext, run() does parse +
    // analyze + execute legacy path (not run_cached).
    ...
}

#[test]
fn select_point_lookup_populates_hint() {
    let mut h = harness::Harness::new();
    h.run("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").unwrap();
    for i in 1..=100 {
        h.run(&format!("INSERT INTO t VALUES ({i}, 'x')")).unwrap();
    }
    h.run("COMMIT").unwrap_or(/* no-op if not in txn */);
    // First SELECT populates the hint.
    let _ = h.run("SELECT v FROM t WHERE id = 50").unwrap();
    assert!(
        h.session.clustered_leaf_hint_present(),
        "first SELECT populates the leaf hint"
    );
}

#[test]
fn select_consecutive_hits_in_same_leaf_reuse_hint() {
    // Two SELECTs hitting same leaf — second uses the hint.
    // Verified by observing that hint.leaf_page_id is the same.
    let mut h = harness::Harness::new();
    h.run("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").unwrap();
    for i in 1..=10 {
        h.run(&format!("INSERT INTO t VALUES ({i}, 'x')")).unwrap();
    }
    let _ = h.run("SELECT v FROM t WHERE id = 5").unwrap();
    let leaf1 = /* read hint.leaf_page_id via test accessor */;
    let _ = h.run("SELECT v FROM t WHERE id = 7").unwrap();
    let leaf2 = /* read hint.leaf_page_id */;
    assert_eq!(leaf1, leaf2, "same leaf reused");
}

#[test]
fn alter_table_invalidates_hint() {
    let mut h = harness::Harness::new();
    h.run("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    h.run("INSERT INTO t VALUES (1)").unwrap();
    let _ = h.run("SELECT id FROM t WHERE id = 1").unwrap();
    assert!(h.session.clustered_leaf_hint_present());
    h.run("ALTER TABLE t ADD COLUMN x INT DEFAULT 0").unwrap();
    // invalidate_table fires inside the resolver path → hint cleared.
    assert!(!h.session.clustered_leaf_hint_present(),
        "ALTER must clear the hint");
}
```

### Implementation outline (per call site)

Replace:
```rust
let row = axiomdb_storage::clustered_tree::lookup(
    storage, Some(root_pid), &pk_key, &snap
)?;
```

With:
```rust
let table_id = resolved.def.id;
let schema_version = resolved.def.schema_version;
let row = axiomdb_storage::clustered_tree::lookup_with_hint(
    storage,
    Some(root_pid),
    &pk_key,
    table_id,
    schema_version,
    ctx.clustered_leaf_hint_slot(),
)?
.filter(|r| r.row_header.is_visible(&snap));
```

(The visibility filter is moved into the caller because
`lookup_with_hint` returns the physical row; callers already do this
or want to do it.)

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql --test integration_cursor_reuse
./tools/vm.sh test -p axiomdb-sql        # broad
./tools/vm.sh clippy axiomdb-sql 2>&1 | tail -5

# Quick measurement (not the hard gate — that's step 5.5):
cargo run -p axiomdb-bench-comparison --release -- \
    --compare --rows 10000 2>&1 | grep -E "Scenario|point_lookup|range_scan"
# Expect point_lookup to improve.
```

### Commit

```
perf(perf-sqlite-gap): step 5.3 — read-path call sites use hint

Switches the six clustered_tree::lookup call sites
(table.rs ×2, delete.rs ×2, update_clustered_helpers.rs,
fk_enforcement.rs, clustered_secondary.rs) to lookup_with_hint,
threading the SessionContext::clustered_leaf_hint_slot().

3 end-to-end tests: SELECT populates hint, consecutive SELECTs in
same leaf reuse it, ALTER invalidates it.

Bench impact (--compare insert_batch unchanged; point_lookup +X×).
```

---

## Step 5.4 — Append fast-path for autocommit clustered INSERT

**Goal:** Autocommit INSERTs into AUTO_INC clustered tables skip the
descent. This is the bench-mover (target: insert_autocommit 5-10×).

**Files:**
- `crates/axiomdb-storage/src/clustered_tree/mod.rs` — add
  `pub fn try_append_with_hint(...)` (wraps `try_insert_rightmost_leaf`
  + hint update)
- `crates/axiomdb-sql/src/executor/insert_clustered.rs` — in the
  autocommit branch (around line ~335 area), call
  `try_append_with_hint` when:
  - The table is clustered
  - The PK is auto_increment
  - The hint exists AND `new_key > hint.max_key`
  - Fall back to the existing path otherwise
- `crates/axiomdb-sql/tests/integration_cursor_reuse.rs` — 3 more
  tests

**Approach:** TDD. Edge-case tests + a perf-smoke test that asserts
2nd autocommit INSERT doesn't descend (via hint stays same leaf).

### Tests to add

```rust
#[test]
fn autocommit_insert_appends_to_hint_leaf() {
    let mut h = harness::Harness::new();
    h.run("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").unwrap();
    // Use autocommit (no BEGIN).
    h.run("INSERT INTO t VALUES (1, 'a')").unwrap();
    let first_leaf = /* hint.leaf_page_id */;
    h.run("INSERT INTO t VALUES (2, 'b')").unwrap();
    let second_leaf = /* hint.leaf_page_id */;
    assert_eq!(first_leaf, second_leaf, "append used the same leaf");
}

#[test]
fn autocommit_insert_non_monotonic_falls_back() {
    let mut h = harness::Harness::new();
    h.run("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    h.run("INSERT INTO t VALUES (100)").unwrap();
    // Smaller key — must NOT go through append fast path.
    h.run("INSERT INTO t VALUES (1)").unwrap();
    // Verify row is actually there (descend worked).
    let rows = h.run("SELECT id FROM t ORDER BY id").unwrap();
    /* assert rows = [1, 100] */
}

#[test]
fn autocommit_insert_into_empty_table_descends_first() {
    let mut h = harness::Harness::new();
    h.run("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    assert!(!h.session.clustered_leaf_hint_present());
    h.run("INSERT INTO t VALUES (1)").unwrap();
    assert!(h.session.clustered_leaf_hint_present(),
        "first insert populates hint via descent");
}
```

### Implementation outline

```rust
// crates/axiomdb-storage/src/clustered_tree/mod.rs

/// Append-mode INSERT that uses the hint when possible.
///
/// If the hint points at a leaf whose `max_key < new_key` (strict),
/// appends directly to that leaf via `try_insert_rightmost_leaf`.
/// Otherwise falls back to `insert` (full descent + split logic).
///
/// Returns `Ok(true)` if the append fast path was taken, `Ok(false)`
/// on fallback. Updates the hint in both cases.
pub fn try_append_with_hint(
    storage: &dyn StorageEngine,
    root_pid: u64,
    new_key: &[u8],
    row_header: RowHeader,
    row_data: &[u8],
    table_id: u32,
    schema_version: u64,
    hint: &mut Option<LeafCursorHint>,
) -> Result<bool, DbError> {
    if let Some(h) = hint.as_ref() {
        if h.table_id == table_id
            && h.root_page_id == root_pid
            && h.schema_version == schema_version
            && new_key > h.max_key.as_slice()
        {
            let appended = try_insert_rightmost_leaf(
                storage, None, h.leaf_page_id,
                &[RightmostAppendRow { key: new_key, row_header, row_data }],
            )?;
            if appended > 0 {
                // Update hint: new max_key is `new_key`.
                let h = hint.as_mut().unwrap();
                h.max_key = new_key.to_vec();
                return Ok(true);
            }
            // Leaf full or other reason — fall through to normal insert.
        }
    }
    // Fallback: standard insert that handles descent + split + hint update.
    insert(storage, ...)?;  // existing function; updates hint via lookup_with_hint
                            // would have been called by the caller; here we
                            // just trigger a full descent.
    Ok(false)
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql --test integration_cursor_reuse
./tools/vm.sh test -p axiomdb-sql
./tools/vm.sh clippy axiomdb-sql 2>&1 | tail -5

cargo run -p axiomdb-bench-comparison --release -- --compare --rows 10000
# Expect insert_autocommit to jump 5-10×.
```

### Commit

```
perf(perf-sqlite-gap): step 5.4 — append fast-path for autocommit INSERT

New storage helper try_append_with_hint that checks the cached leaf
hint; if the new key is strictly greater than max_key, appends
directly via try_insert_rightmost_leaf (skipping the B-tree descent).

Wired into the autocommit branch of insert_clustered_ctx for
AUTO_INCREMENT / monotonic-PK INSERTs.

3 tests: append uses same leaf, non-monotonic falls back correctly,
empty-table first insert descends.

Bench: insert_autocommit jumps from 8.7K to ?K rows/s.
```

---

## Step 5.5 — Measure + close

**Goal:** Verify spec done-criteria; update docs/memory; final commit.

### Verification against spec

- [ ] `axiomdb_bench --compare --rows 10000` shows
      `insert_autocommit ≥ 50K rows/s` (≥ 5× baseline 8.7K)
- [ ] `point_lookup ≥ 13K ops/s` (≥ 1.5× baseline 8.9K)
- [ ] `range_scan ≥ 1.2M rows/s` (≥ 1.7× baseline 727K)
- [ ] No regression on `insert_batch`, `full_scan`, `select_where`,
      `group_by` (within ±5%)
- [ ] `cargo nextest run --workspace` (Lima) — clean
- [ ] `cargo clippy --workspace -- -D warnings` (Lima) — clean
- [ ] `cargo fmt --check` — clean
- [ ] `tools/wire-test.py` — clean (pre-flight per memory)
- [ ] 11+ new tests in `integration_cursor_reuse.rs` (3 unit + 8
      integration)
- [ ] Rustdoc on `LeafCursorHint`, `lookup_with_hint`,
      `try_append_with_hint`, and the 4-5 new methods on SessionContext

### Docs to update

- `docs/perf-sqlite-gap.md` — new section "Attack 5" with before/after
- `memory/project_sqlite_baseline.md` — append row

### Final commit

```
feat(perf-sqlite-gap): close Attack 5 — cursor reuse cross-statement

Implements specs/fase-perf-sqlite-gap/spec-cursor-reuse-cross-statement.md
Plan: specs/fase-perf-sqlite-gap/plan-cursor-reuse-cross-statement.md

Results (axiomdb_bench --compare --rows 10000):
  Scenario              Before    After    Δ
  insert_autocommit     8.7K      ?K       ?×
  point_lookup          8.9K      ?K       ?×
  range_scan            727K      ?M       ?×
  crud/delete           1.09M     ?M       ?×
  insert_batch          20.7K     ?K       (target flat)

Tests: 11+ integration tests covering all spec edge cases.
All 4351+ workspace tests pass, clippy + fmt clean, wire smoke green.
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| `LeafCursorHint` becomes stale after concurrent split | Medium | The hot path re-reads the page bytes; `leaf_search_checked` returning out-of-range triggers fall-back to descent. False-positive cost: 1 extra page read. |
| `read_page` on a freed leaf returns garbage | Low | `clustered_page_type` check at the top of the hot path catches non-leaf pages; falls back to descent. |
| Append fast-path corrupts the tree on edge cases (page full but undetected) | Medium | `try_insert_rightmost_leaf` returns 0 on failure → fall back to normal `insert`. Existing function already tested for these cases. |
| Test harness needs hint accessor that exposes the leaf_page_id internals | Low | Add `clustered_leaf_hint_leaf_id_for_test() -> Option<u64>` on `SessionContext` behind a `#[cfg(test)]` or `#[doc(hidden)]` — small surface, only for tests. |
| Wire test path regression because run_inner doesn't use the hint at all | Low | Step 5.3 wires the lookup at the storage call sites; wire path goes through the same call sites; nothing custom needed. |
| Step 5.4's `insert_clustered_ctx` rewrite breaks existing INSERT tests | Medium | TDD per integration test; broad axiomdb-sql suite run after each commit. Worst-case: revert just step 5.4 and ship steps 5.1-5.3 alone (still helps point_lookup). |

## Rollback plan

1. Each step has its own commit, individually revertible:
   - Steps 5.1, 5.2 are pure additions — revert with zero impact.
   - Step 5.3 changes call sites; revert restores legacy lookup.
   - Step 5.4 adds the append path; revert restores per-row descent.
2. If the whole attack is abandoned:
   `git branch abandoned/plan-cursor-reuse-cross-statement-2026-05-17`
   from the last clean commit; revert spec to `draft` with a note.

## Estimated effort

Total: ~2-3 days.
- Step 5.1 (storage helper + 4 unit tests): 0.5 day
- Step 5.2 (SessionContext slot + 4 unit tests): 0.5 day
- Step 5.3 (wire 6 call sites + 3 integration tests): 1 day
- Step 5.4 (append fast path + 3 tests): 0.5 day
- Step 5.5 (measure + docs + close): 0.5 day
