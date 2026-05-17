# Plan: insert-setup-dedup-B — clone removal in INSERT hot path

Phase: perf-sqlite-gap — close embedded INSERT gap with SQLite
Task: B — eliminate per-statement and per-row clones in INSERT path
Spec: specs/fase-perf-sqlite-gap/spec-insert-setup-dedup-B.md
Status: done (B.1/B.2/B.3 done; B.4/B.5 deferred as noise-level)

## Summary

Six small steps. **Steps 1, 2, 4, 5** are pure refactors — they remove
specific clones without changing behavior. The existing 4351-test
workspace suite is the safety net (no new tests needed; if a refactor
breaks correctness the suite catches it). **Step 3** adds the
`col_positions` cache with its own dedicated tests (new API + new
behavior). **Step 6** is the closing protocol: measurement against
spec budgets, workspace gates, docs.

Order minimizes diff size per step and lets the diagnostic harness
attribute each step's win individually — easier to revert and to
explain in commits.

## Dependencies

Must be done first:
- [x] spec-insert-setup-dedup-B approved (commit `99d32843`)
- [x] Attack 3.A landed (commit `50930d99`)
- [x] Diagnostic harness `--diagnose-insert` / `--diagnose-insert-deep`
      (commit `f8037b4c`)

Blocks (until done):
- Attack 2 (statement fingerprinting) — Attack 2's cached payload is
  leaner once these clones are gone, less to capture per shape.

## Affected files

New files:
- (none — tests go into the existing
  `crates/axiomdb-sql/tests/integration_resolve_table_cache.rs`)

Modified files:
- `crates/axiomdb-sql/src/executor/shared.rs` — B.1 (search_path
  ref) + B.3 (col_positions cache invalidation on table eviction)
- `crates/axiomdb-sql/src/executor/insert_clustered_ctx.rs` —
  B.2 (primary_idx ref) + B.3 (col_positions cache lookup) + B.4
  (primary_key_bytes move) + B.5 (same in autocommit path lines
  ~35-220)
- `crates/axiomdb-sql/src/session.rs` — B.3
  (`get_insert_col_positions` / `cache_insert_col_positions` + storage
  HashMap on SessionContext)
- `crates/axiomdb-sql/tests/integration_resolve_table_cache.rs` —
  B.3 tests + new module-level coverage of col_positions invariants
- `docs/perf-sqlite-gap.md` — Step 6 update with new numbers
- `memory/project_sqlite_baseline.md` — Step 6 update

---

## Step 1 — Remove `ctx.search_path.clone()` in `resolve_table_cached`

**Goal:** Stop allocating a fresh `Vec<String>` of length 1-2 on
every single `resolve_table_cached` call. Iterate `&ctx.search_path`
directly.

**Files:** `crates/axiomdb-sql/src/executor/shared.rs`

**Approach:** Pure refactor. The existing 14 tests in
`integration_resolve_table_cache.rs` plus the full workspace suite
(4351 tests) verify correctness.

### The change

```rust
// Today (shared.rs:42):
let search_path: Vec<String> = ctx.search_path.clone();
for schema in &search_path { … }

// After:
// Take a snapshot reference. ctx is not mutated until inside the loop
// body via cache_table — that's a different field than search_path,
// so we can hold &ctx.search_path safely.
//
// If the borrow checker complains because cache_table needs &mut ctx,
// the fix is to extract the schema names by reference into a small
// local Vec<&str> (no String allocation) just before the loop.
for schema_idx in 0..ctx.search_path.len() {
    let schema = ctx.search_path[schema_idx].clone();  // last resort
    …
}
```

Likely outcome: the local-`Vec<&str>` form works without contortion.
If `&str` lifetime conflicts with later `&mut ctx`, fall back to
indexing + `.clone()` of one short String per iteration, which still
allocates 0 most of the time (search_path is usually 1 entry: "public").

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql --test integration_resolve_table_cache
./tools/vm.sh test -p axiomdb-sql       # broad safety net
./tools/vm.sh clippy axiomdb-sql 2>&1 | tail -5

cargo run -p axiomdb-bench-comparison --release -- \
    --scenario insert_batch --rows 10000 --diagnose-insert | tail -10
# Expect execute_with_ctx per row to drop 1-2 µs.
```

### Commit

```
perf(perf-sqlite-gap): step 1 — drop search_path.clone() per resolve

resolve_table_cached was allocating a fresh Vec<String> on every
call. search_path is almost always 1 entry ('public'), but at 21K
inserts/s that adds up to ~21K small Vec allocations per second.

Iterate by reference instead. No behavior change. 4351/4351 tests pass.
```

---

## Step 2 — Remove `primary_idx.clone()` in batched INSERT path

**Goal:** Stop cloning the batch's `primary_idx: IndexDef` on every
INSERT statement. The clone exists because of a `ctx.clustered_insert_batch.as_mut()`
borrow conflict downstream — solve it instead.

**Files:** `crates/axiomdb-sql/src/executor/insert_clustered_ctx.rs`

**Approach:** Pure refactor.

### The change

Today (line ~374-379):
```rust
let primary_idx = ctx
    .clustered_insert_batch
    .as_ref()
    .unwrap()
    .primary_idx
    .clone();
```

The clone is needed because later in the per-row loop we call
`prepare_row_with_ctx(full_values, schema_cols, &primary_idx, …, ctx, …)`
and that takes `&mut ctx`. Can't hold `&batch.primary_idx` (which
implies `&ctx`) at the same time as `&mut ctx`.

Fix options (try in order):
1. Wrap `ClusteredInsertBatch.primary_idx` in `Arc<IndexDef>` so we
   can cheaply `Arc::clone` (one atomic increment vs full Vec\<IndexColumnDef\>
   clone). Most ergonomic.
2. Pull `primary_idx` out of `batch` by ownership at the top of the
   per-row loop, restore at end (awkward).
3. Refactor `prepare_row_with_ctx` to take an immutable cursor instead
   of `&mut ctx` for this case (deepest, riskiest).

Going with option 1. `IndexDef` is already cloneable and small but
the `columns: Vec<IndexColumnDef>` does allocate. Arc avoids that.

### Implementation outline

```rust
// crates/axiomdb-sql/src/session.rs — adjust the field type
pub struct ClusteredInsertBatch {
    pub primary_idx: std::sync::Arc<axiomdb_catalog::IndexDef>,
    …
}

// crates/axiomdb-sql/src/executor/insert_clustered_ctx.rs — at init (~line 359):
ctx.clustered_insert_batch = Some(crate::session::ClusteredInsertBatch {
    primary_idx: std::sync::Arc::new(primary_idx),
    …
});

// Replace the .clone() with Arc::clone (cheap atomic op):
let primary_idx = std::sync::Arc::clone(
    &ctx.clustered_insert_batch.as_ref().unwrap().primary_idx
);

// In the per-row loop, `&*primary_idx` gives `&IndexDef` to
// prepare_row_with_ctx — no signature change needed at the callee.
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql
./tools/vm.sh clippy axiomdb-sql 2>&1 | tail -5
cargo run -p axiomdb-bench-comparison --release -- \
    --scenario insert_batch --rows 10000 --diagnose-insert | tail -10
# Expect execute_with_ctx per row to drop another 1-2 µs.
```

### Commit

```
perf(perf-sqlite-gap): step 2 — Arc<IndexDef> avoids primary_idx clone

Cloning IndexDef per INSERT statement (the entry to the batched path)
allocates its inner Vec<IndexColumnDef> plus the predicate string.
Wrap in Arc so the per-statement reuse is one atomic increment.
```

---

## Step 3 — Cache `col_positions` by `(table_id, columns_signature)`

**Goal:** Stop re-computing `build_insert_column_positions` (allocates
a `Vec<usize>` + does name lookups) on every INSERT into the same
table with the same column shape.

**Files:**
- `crates/axiomdb-sql/src/session.rs` — new cache + API
- `crates/axiomdb-sql/src/executor/insert_clustered_ctx.rs` — call
  sites in BOTH `enqueue_clustered_insert_ctx` AND
  `execute_clustered_insert_ctx`
- `crates/axiomdb-sql/src/executor/shared.rs` — invalidate
  col_positions when `invalidate_table` runs
- `crates/axiomdb-sql/tests/integration_resolve_table_cache.rs` — new
  tests

**Approach:** TDD — write the cache-hit and cache-miss tests first,
then add the API + plumbing.

### Tests to add

```rust
#[test]
fn col_positions_cached_across_inserts_same_shape() {
    let mut h = harness::Harness::new();
    h.run("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").unwrap();
    for i in 1..=10 {
        h.run(&format!("INSERT INTO t VALUES ({i}, 'a')")).unwrap();
    }
    // Exactly one (table_id, sig=0) entry; sig=0 = "all columns".
    assert_eq!(h.session.insert_col_positions_count(), 1);
}

#[test]
fn col_positions_distinct_for_distinct_column_lists() {
    let mut h = harness::Harness::new();
    h.run("CREATE TABLE t (a INT PRIMARY KEY, b INT, c INT)").unwrap();
    h.run("INSERT INTO t(a, b) VALUES (1, 2)").unwrap();
    h.run("INSERT INTO t(a, c) VALUES (3, 4)").unwrap();
    // Two entries for the same table — different column lists.
    assert_eq!(h.session.insert_col_positions_count(), 2);
}

#[test]
fn col_positions_evicted_on_schema_bump() {
    let mut h = harness::Harness::new();
    h.run("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    h.run("INSERT INTO t VALUES (1)").unwrap();
    assert_eq!(h.session.insert_col_positions_count(), 1);
    h.run("ALTER TABLE t ADD COLUMN x INT DEFAULT 0").unwrap();
    // schema_version bump propagates to col_positions cache too.
    assert_eq!(h.session.insert_col_positions_count(), 0);
    h.run("INSERT INTO t VALUES (2, 99)").unwrap();
    assert_eq!(h.session.insert_col_positions_count(), 1);
}

#[test]
fn col_positions_isolated_per_table() {
    let mut h = harness::Harness::new();
    h.run("CREATE TABLE t1 (id INT PRIMARY KEY)").unwrap();
    h.run("CREATE TABLE t2 (id INT PRIMARY KEY)").unwrap();
    h.run("INSERT INTO t1 VALUES (1)").unwrap();
    h.run("INSERT INTO t2 VALUES (1)").unwrap();
    h.run("INSERT INTO t1 VALUES (2)").unwrap();
    assert_eq!(h.session.insert_col_positions_count(), 2);
}
```

### Implementation outline

```rust
// crates/axiomdb-sql/src/session.rs
pub struct SessionContext {
    …
    /// Phase 3.B: cached per-(table_id, columns_signature) result of
    /// build_insert_column_positions. Skips ~1-3 µs per INSERT call.
    /// Invalidated alongside the ResolvedTable cache when the table's
    /// schema_version changes.
    insert_col_positions: HashMap<(u32, u64), Vec<usize>>,
}

impl SessionContext {
    pub fn get_insert_col_positions(
        &self,
        table_id: u32,
        columns_signature: u64,
    ) -> Option<&Vec<usize>> {
        self.insert_col_positions.get(&(table_id, columns_signature))
    }

    pub fn cache_insert_col_positions(
        &mut self,
        table_id: u32,
        columns_signature: u64,
        col_positions: Vec<usize>,
    ) {
        self.insert_col_positions.insert(
            (table_id, columns_signature),
            col_positions,
        );
    }

    /// Test/diagnostic accessor.
    pub fn insert_col_positions_count(&self) -> usize {
        self.insert_col_positions.len()
    }

    /// Invalidates ALL col_positions entries for a given table_id.
    /// Called from invalidate_table.
    pub fn invalidate_insert_col_positions(&mut self, table_id: u32) {
        self.insert_col_positions
            .retain(|(t, _), _| *t != table_id);
    }
}

// Update invalidate_table to also call invalidate_insert_col_positions:
pub fn invalidate_table(&mut self, database: &str, schema: &str, table: &str) {
    if let Some(resolved) = self.cache.get(&Self::key(database, schema, table)) {
        let id = resolved.def.id;
        self.heap_tail.remove(&id);
        self.invalidate_insert_col_positions(id);
    }
    self.cache.remove(&Self::key(database, schema, table));
}
```

```rust
// crates/axiomdb-sql/src/executor/insert_clustered_ctx.rs
// Helper: hash stmt.columns into a u64 (use ahash or default hasher
// — `Option<&Vec<String>>` → 0 for None, else a stable hash).
fn columns_signature(columns: Option<&Vec<String>>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    match columns {
        None => 0u8.hash(&mut h),
        Some(c) => {
            1u8.hash(&mut h);
            c.hash(&mut h);
        }
    }
    h.finish()
}

// Replace this:
let col_positions =
    build_insert_column_positions(schema_cols, &stmt.columns, …)?;

// With this:
let sig = columns_signature(stmt.columns.as_ref());
let col_positions: std::sync::Arc<Vec<usize>> = match ctx
    .get_insert_col_positions(resolved.def.id, sig)
{
    Some(cached) => std::sync::Arc::new(cached.clone()),  // cache hit
    None => {
        let computed = build_insert_column_positions(schema_cols, &stmt.columns, …)?;
        ctx.cache_insert_col_positions(resolved.def.id, sig, computed.clone());
        std::sync::Arc::new(computed)
    }
};
```

The `Arc::new(cached.clone())` looks like a clone but `cached` is
`&Vec<usize>` — we're cloning the Vec<usize> which is small (size =
number of columns × 8 bytes). Cheaper than building it from scratch
which does a name lookup per column.

Alternative: change the cache to store `Arc<Vec<usize>>` directly,
return `Arc::clone`. Cleaner but pulls Arc into more public surface.
Decide at implementation time.

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql --test integration_resolve_table_cache
./tools/vm.sh test -p axiomdb-sql
./tools/vm.sh clippy axiomdb-sql 2>&1 | tail -5
cargo run -p axiomdb-bench-comparison --release -- \
    --scenario insert_batch --rows 10000 --diagnose-insert | tail -10
# Expect execute_with_ctx per row to drop another 2-4 µs.
```

### Commit

```
perf(perf-sqlite-gap): step 3 — cache col_positions per (table, shape)

build_insert_column_positions allocates a Vec<usize> and does a name
lookup per column on every INSERT. Cache the result in SessionContext
keyed by (table_id, columns_signature). Invalidated via the existing
invalidate_table path (which now also clears the col_positions slot).

4 new integration tests cover hit, distinct shapes, eviction on DDL,
and isolation across tables.
```

---

## Step 4 — Move `primary_key_bytes` once per row in batch push

**Goal:** Stop cloning `prepared.primary_key_bytes` (a `Vec<u8>`)
TWICE per row in `enqueue_clustered_insert_ctx`. With 10K rows that
is 20K small Vec allocations per batch.

**Files:** `crates/axiomdb-sql/src/executor/insert_clustered_ctx.rs`
(lines ~516-517)

**Approach:** Pure refactor. The existing tests + the in-bench-batch
duplicate-PK test cover correctness.

### The change

Today (lines 514-525):
```rust
crate::time_insert_phase!(batch_push_ns, {
    let batch = ctx.clustered_insert_batch.as_mut().unwrap();
    batch.staged_pks.insert(prepared.primary_key_bytes.clone());
    statement_staged_pks.push(prepared.primary_key_bytes.clone());
    batch.rows.push(crate::session::StagedClusteredRow {
        values: prepared.values,
        encoded_row: prepared.encoded_row,
        primary_key_values: prepared.primary_key_values,
        primary_key_bytes: prepared.primary_key_bytes,
    });
});
```

After: insert into the HashSet by reference once for the duplicate
check, then move the bytes into both `statement_staged_pks` and the
StagedClusteredRow. Actually we still need TWO copies of the bytes
(one in `batch.staged_pks`, one in `StagedClusteredRow.primary_key_bytes`).
What we can avoid is the THIRD copy currently held in
`statement_staged_pks` — that vec only exists for rollback-this-statement
in case of an intra-batch dup later, so we can store INDICES into
`batch.staged_pks` order instead of full bytes.

Refactor approach:
```rust
// Old: Vec<Vec<u8>> of pks staged within this single statement
let mut statement_staged_pks: Vec<Vec<u8>> = Vec::new();

// New: just remember how many we added; rollback truncates from the end.
let statement_start_len_in_set = batch.staged_pks.len();
// On dup: roll back by removing the last (batch.staged_pks.len() -
// statement_start_len_in_set) entries — but HashSet has no "last N"
// concept. So track indices a different way:
let mut statement_staged_pks: Vec<u64> = Vec::new();  // just hashes
…
batch.staged_pks.insert(prepared.primary_key_bytes.clone());
statement_staged_pks.push(hash_of(&prepared.primary_key_bytes));
batch.rows.push(StagedClusteredRow { primary_key_bytes: prepared.primary_key_bytes, … });

// On rollback (intra-batch dup detected):
for h in &statement_staged_pks { batch.staged_pks.remove_by_hash(h); }  // pseudocode
```

Hmm this is uglier than I expected. Let me simplify: the cleanest
removal is to take ownership of `prepared.primary_key_bytes` exactly
once (move into `StagedClusteredRow`) and only clone for the HashSet
insert. That removes 1 of the 2 clones (the `statement_staged_pks`
one).

```rust
// New:
crate::time_insert_phase!(batch_push_ns, {
    let batch = ctx.clustered_insert_batch.as_mut().unwrap();
    let pk_bytes = prepared.primary_key_bytes;  // moved
    batch.staged_pks.insert(pk_bytes.clone());  // 1st clone (HashSet ownership)
    statement_staged_pks.push(pk_bytes.clone()); // 2nd clone (kept for now)
    batch.rows.push(crate::session::StagedClusteredRow {
        values: prepared.values,
        encoded_row: prepared.encoded_row,
        primary_key_values: prepared.primary_key_values,
        primary_key_bytes: pk_bytes,  // moved
    });
});
```

Net: 3 references → 2 clones + 1 move = same count. Actually no win
here without restructuring.

Better: use indices into `batch.rows` for `statement_staged_pks`:

```rust
let mut statement_staged_pk_row_indices: Vec<usize> = Vec::new();
…
let row_index = batch.rows.len();
batch.staged_pks.insert(pk_bytes.clone());        // 1 clone for HashSet
statement_staged_pk_row_indices.push(row_index);
batch.rows.push(StagedClusteredRow {
    primary_key_bytes: pk_bytes,                  // 1 move
    …
});

// On rollback (intra-batch dup):
for idx in &statement_staged_pk_row_indices {
    let bytes = &batch.rows[*idx].primary_key_bytes;
    batch.staged_pks.remove(bytes);
}
batch.rows.truncate(statement_start_len);
```

That eliminates the 2nd per-row clone entirely. Net savings: ~10K Vec\<u8\>
allocations per 10K-row batch. Small per-call but real.

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql --test integration_insert_clustered_batch
./tools/vm.sh test -p axiomdb-sql  # broad
./tools/vm.sh clippy axiomdb-sql 2>&1 | tail -5
cargo run -p axiomdb-bench-comparison --release -- \
    --scenario insert_batch --rows 10000 --diagnose-insert-deep \
    --features bench-timings | tail -20
# Expect "batch push" per-row µs to drop ~30-50%.
```

### Commit

```
perf(perf-sqlite-gap): step 4 — single PK-bytes ownership in batch push

Was cloning prepared.primary_key_bytes twice per row (once for the
HashSet, once for statement-scope rollback bookkeeping). Replace the
rollback vec with row indices into batch.rows; on rollback look up the
bytes there.

Saves ~10K Vec<u8> allocations per 10K-row batch.
```

---

## Step 5 — Apply same removals to `execute_clustered_insert_ctx` (autocommit path)

**Goal:** The autocommit-INSERT path (used by `--scenario
insert_autocommit`) has the same clones as the batched path. Apply
the same fixes.

**Files:** `crates/axiomdb-sql/src/executor/insert_clustered_ctx.rs`
(lines ~35-220)

**Approach:** Pure refactor. Apply patterns from steps 2 + 3 + 4.

### The change

Audit the autocommit path against the clones we removed:
- Line 44: `primary_index(...).clone()` → `Arc<IndexDef>` if the path
  benefits (single statement, may not be worth the wrap).
- Line 49: `.cloned()` on secondary indexes — kept (need owned for
  later mutations).
- Line 67: `build_insert_column_positions` → use the cache from
  Step 3.
- Other in-call clones (line 343, 348, 500): re-audit; some may have
  been side-effects of code shape that B.2/B.3 already removed.

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql
./tools/vm.sh clippy axiomdb-sql 2>&1 | tail -5

cargo run -p axiomdb-bench-comparison --release -- \
    --scenario insert_autocommit --rows 1000 --diagnose-insert | tail -10
# Expect autocommit INSERT ops/s to improve 20-30%.
```

### Commit

```
perf(perf-sqlite-gap): step 5 — apply clone removal to autocommit path

execute_clustered_insert_ctx had the same per-statement clones as the
batched path. Reuses the col_positions cache from step 3, swaps clone
for Arc::clone where it pulls weight. Confirmed with --scenario
insert_autocommit.
```

---

## Step 6 — Measure against budgets, close

**Goal:** Verify every spec done-criterion.

### Verification against spec

- [ ] `axiomdb_bench --compare --rows 10000` shows `insert_batch ≥ 28K
      rows/s` (was 21K → +30%).
- [ ] `axiomdb_bench --diagnose-insert --rows 10000` shows
      `execute_with_ctx per row ≤ 30 µs` (was ~44 µs).
- [ ] Other scenarios (full_scan, select_where, point_lookup,
      count_star, range_scan) improve OR stay flat — record numbers.
- [ ] `cargo nextest run --workspace` (Lima) — clean.
- [ ] `cargo clippy --workspace -- -D warnings` (Lima) — clean.
- [ ] `cargo fmt --check` — clean.
- [ ] `tools/wire-test.py` — clean (per memory: pkill +
      cargo build -p axiomdb-server pre-flight).
- [ ] 4 new tests in `integration_resolve_table_cache.rs` (from B.3)
      all pass.
- [ ] Two `#[ignore]` tests from Attack 3.A remain ignored (no new
      TODOs).

### Docs to update

- `docs/perf-sqlite-gap.md` — new section "Attack 3.B — Clone removal"
  with before/after numbers.
- `memory/project_sqlite_baseline.md` — update with post-3.B numbers.

### Final commit

```
feat(perf-sqlite-gap): close Attack 3.B — clone removal in INSERT hot path

Implements specs/fase-perf-sqlite-gap/spec-insert-setup-dedup-B.md
Plan: specs/fase-perf-sqlite-gap/plan-insert-setup-dedup-B.md

Results (axiomdb_bench --compare --rows 10000):
  insert_batch:  21K  →  ?K  rows/s   (target ≥ 28K)
  Gap vs SQLite: 47×  →  ?×           (target ≤ 35×)
  execute_with_ctx per row: 44 µs → ? µs  (target ≤ 30 µs)

Tests: 4 new integration tests for col_positions cache invariants.
All 4351+ workspace tests pass, clippy + fmt clean.
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| `Arc<IndexDef>` change in `ClusteredInsertBatch` requires `&IndexDef` deref at many sites | Medium | Step 2 — `&*arc` gives `&IndexDef` at zero cost. If a callee signature requires owned `IndexDef` (unlikely), revisit by passing `Arc<IndexDef>` through. |
| `columns_signature` hash collisions corrupt across tables | Very low | Cache is keyed by `(table_id, signature)` so different tables can't collide. Inside a table, two different shapes producing the same hash is astronomically unlikely with `DefaultHasher`; even if it happened it would only cause a wrong `col_positions` Vec to be reused, surfacing as a `ColumnNotFound` error on insert. Test `col_positions_distinct_for_distinct_column_lists` would catch the trivial case. |
| Rollback path for `statement_staged_pks` using row indices misses rows that were already pushed before an error | Medium | The old path already has this pattern (it truncates `batch.rows`). The index list just records which rows were added in this statement; truncating `batch.rows` to `statement_start_len` removes the same rows. Test in step 4. |
| `Vec<usize>` clone in `col_positions` cache hit still allocates | Low | Acceptable — Vec<usize> of typical size 5-10 is sub-µs. Alternatively store `Arc<Vec<usize>>` directly. Decide at implementation time. |
| Refactor exposes latent borrow-checker pain in the autocommit path | Medium | Step 5 is the riskiest step. If the autocommit path's structure fights the changes, revert just Step 5 (keep 1-4) and document. |
| Wire test pre-flight forgotten → false-positive failure | Low | Step 6 verification list mentions the memory rule explicitly. |

## Rollback plan

1. Each step is its own commit. To abandon a single step:
   `git revert <step-N hash>`. All later steps depend only on the
   steps named in their "Files" section.
2. If a step reveals the design is wrong:
   `git branch abandoned/plan-insert-setup-dedup-B-2026-05-16` from
   the last clean commit, revert the spec to `draft` with a note.

## Estimated effort

Total: ~1.5 days for B.1-B.5 + tests + closing.
- Step 1 (search_path): 20 min
- Step 2 (Arc<IndexDef>): 1 h
- Step 3 (col_positions cache + 4 tests): 2.5 h
- Step 4 (PK bytes single ownership): 1 h
- Step 5 (autocommit path same): 2 h
- Step 6 (measure + docs + final): 1.5 h
