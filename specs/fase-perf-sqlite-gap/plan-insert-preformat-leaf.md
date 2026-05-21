# Plan: insert-preformat-leaf

Phase: perf-sqlite-gap — close embedded write gap with SQLite
Task: bypass the per-row `OwnedLeafCell` on the clustered rightmost-leaf fast path
Spec: specs/fase-perf-sqlite-gap/spec-insert-preformat-leaf.md
Status: in progress

## Summary

One real code change, scoped to the rightmost-leaf fast path
`try_insert_rightmost_leaf_batch`
([`mod.rs:361`](crates/axiomdb-storage/src/clustered_tree/mod.rs:361)). Today it
calls `materialize_leaf_cell` (which does `key.to_vec()` +
`row_data[..local_len].to_vec()` into an `OwnedLeafCell`) and then
`insert_cell_with_overflow` copies those slices into the page. We replace the
owned-cell construction with: validate → compute `local_len` → write the overflow
chain only if the row exceeds the leaf-local capacity → call
`insert_cell_with_overflow` with **slices of the caller's `encoded_row`**. Copy
count 3 → 2; allocs/row −2 for no-overflow rows. Byte-identical page output.

### Implementation note — inline, not a separate helper

The spec sketched an `insert_preformatted_leaf_cell` helper, but the defrag-retry
shares `&mut page` and the per-call `defragmented` flag (mod.rs:391) across rows,
so the change is cleanest **inline** in the existing `for row in rows` loop: keep
the current `Ok/HeapPageFull-retry/free_chain/break` control-flow shape and only
swap the cell *source* from `OwnedLeafCell` fields to borrowed slices +
`overflow_first_page`. Minimal diff = easiest to verify byte-identical.

## Affected files

Modified:
- `crates/axiomdb-storage/src/clustered_tree/mod.rs` — `try_insert_rightmost_leaf_batch`:
  drop `materialize_leaf_cell`; inline chain-write + slice pass-through.
- `crates/axiomdb-storage/src/clustered_tree/tests_insert.rs` — add the
  rightmost-append regression test (Step 1).

Unchanged (explicitly): `materialize_leaf_cell` / `OwnedLeafCell` stay (general
insert + split/rebalance still use them); `insert_cell_with_overflow`,
`clustered_overflow::*`, page format, WAL records.

## Steps

### Step 1 — TDD: lock the fast path with a mixed small+overflow test
Add `test_rightmost_append_batch_preformat_small_and_overflow` to
`tests_insert.rs`: build a clustered leaf, then `try_insert_rightmost_leaf_batch`
a batch of ascending-key rows where some `row_data` fit inline and at least one
exceeds `local_row_len` (forces `write_chain`). Assert every row reads back via
`lookup_physical` with exact key + full `row_data` (covers the overflow
reconstruction path). This must pass BEFORE and AFTER Step 2 (behavior unchanged).
- Verify: `./tools/vm.sh test -p axiomdb-storage tests_insert`

### Step 2 — Replace materialize_leaf_cell with slice pass-through
In `try_insert_rightmost_leaf_batch`, inside the `for row in rows` loop:
- Keep the existing `validate_row_payload(row.key, row.row_data)?` (line 394) and
  the `row.key <= prev_key` ordering guard.
- Remove the `materialize_leaf_cell(...)` call. Instead:
  ```rust
  let local_len = clustered_leaf::local_row_len(row.key.len(), row.row_data.len());
  let overflow_first_page = if row.row_data.len() > local_len {
      clustered_overflow::write_chain(storage, batch.as_deref_mut(), &row.row_data[local_len..])?
  } else {
      None
  };
  let local_row_data = &row.row_data[..local_len];
  ```
- In both `insert_cell_with_overflow` calls (initial + post-defrag), pass
  `row.key`, `row.row_header`, `row.row_data.len()`, `local_row_data`,
  `overflow_first_page`.
- In every `Err(HeapPageFull)` / `Err(e)` arm, free the chain via
  `overflow_first_page` (was `cell.overflow_first_page`).
- Verify (compiles + unit): `./tools/vm.sh test -p axiomdb-storage`

### Step 3 — A/B benchmark (macOS, the win signal)
Build the bench on macOS, A/B against the pre-change binary (medians of 3+,
interleaved):
- `cargo build --release -p axiomdb-bench-comparison`
- `target/release/axiomdb_bench --scenario insert_appender_large --rows 50000`
  (fast path 100% → clearest signal)
- `target/release/axiomdb_bench --scenario insert_batch --rows 10000`
- `AXIOMDB_DEBUG_CLUSTERED_INSERT=1 target/release/axiomdb_bench --scenario insert_batch --rows 2000`
  → `tree_ms` lower, `fast_path_hits` unchanged.
Record before/after in the close report. Honest: this is one slice of the parity
stack; expect a modest but real `tree_ms` reduction, larger on the Appender.

### Step 4 — Close (workspace gates, Lima)
- `./tools/vm.sh test --workspace` (run alone) — clustered tree suites +
  `axiomdb-network::integration_open_integrity` (crash recovery) green.
- `./tools/vm.sh clippy --workspace -- -D warnings`
- `./tools/vm.sh fmt-check`
- Confirm no read regression (`--compare` reads are write-path-independent).
- Per CLAUDE.md close protocol: docs (benchmarks.md write section + numbers),
  progreso.md, memory (project_insert_perf), commit (no Co-Authored-By), push.

## Risk register

- **Slice lifetime / borrow:** `&row.row_data[..local_len]` borrows from
  `RightmostAppendRow.row_data` (which borrows `PreparedClusteredInsertRow.encoded_row`),
  alive for the whole call → no lifetime issue; `insert_cell_with_overflow` copies
  before returning.
- **Defrag retry re-writing the chain:** chain is written ONCE before the first
  attempt; both attempts reuse `overflow_first_page`. Mirror the current code
  exactly. Covered by the overflow test (Step 1) + an over-capacity page case.
- **Byte-identical regression:** any divergence is caught by the existing
  `tests_insert`/`tests_lookup`/`tests_range` (insert→read round-trips) + crash
  recovery replay. If they pass, the page bytes are unchanged.
