# Plan: copy-streaming

Phase: 20 — Types + import/export
Task: COPY FROM streaming — O(batch) memory for CSV and JSONL
Spec: specs/fase-20/spec-copy-streaming.md
Status: in-progress

## Summary

Three steps. Step 1 adds tests for the batch-loop behavior (empty file, exact batch
size, batch+1), then refactors `execute_copy_from` so CSV is streamed in batches of
1024 rows rather than collected wholesale. Step 2 adds tests for schema-first JSONL
streaming (unknown keys ignored, missing keys → NULL, error mid-stream rolls back),
then implements the schema-first single-pass batch loop for JSONL. Step 3 adds wire
smoke assertions, updates docs, and closes the subphase.

All changes are confined to `crates/axiomdb-sql/src/executor/copy_from.rs` and the
existing test file `crates/axiomdb-sql/tests/integration_copy.rs`.

## Dependencies

Must be done first:
- [x] spec-copy-streaming approved
- [x] Phase 20.5 COPY FROM baseline in place

Blocks:
- nothing

## Affected files

Modified:
- `crates/axiomdb-sql/src/executor/copy_from.rs` — batch loop + JSONL schema-first
- `crates/axiomdb-sql/tests/integration_copy.rs` — new streaming tests
- `tools/wire-test.py` — 2 new 20.8 assertions
- `docs/progreso.md` — mark 20.8 ✅
- `docs/fase-20.md` — add 20.8 section
- `docs-site/src/user-guide/sql-reference/ddl.md` — COPY FROM streaming note
- `memory/project_state.md` — update

---

## Step 1 — CSV batch-streaming

**Goal:** Replace the collect-all-then-insert pattern for CSV with a batch loop
that holds at most 1024 rows in memory at any time.

**Files:** `copy_from.rs`, `integration_copy.rs`

### Tests to add

```rust
// crates/axiomdb-sql/tests/integration_copy.rs

#[test]
fn test_copy_from_csv_empty_file() {
    // Empty CSV with header only → count 0, no error
    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("empty.csv");
    std::fs::write(&csv, "id,name\n").unwrap();
    let storage = MemoryStorage::new();
    // ... setup table, run COPY FROM, assert count == 0
}

#[test]
fn test_copy_from_csv_exact_batch_size() {
    // Exactly 1024 rows → one full batch, count == 1024
    // (generates CSV programmatically with tempfile)
}

#[test]
fn test_copy_from_csv_batch_plus_one() {
    // 1025 rows → full batch + partial batch of 1, count == 1025
}
```

### Implementation outline

```rust
// copy_from.rs

const COPY_BATCH_SIZE: usize = 1024;

// New: shared batch-insert helper used by all format loops
fn flush_batch(
    batch: &mut Vec<Vec<axiomdb_types::Value>>,
    columns: Option<Vec<String>>,
    table: &str,
    exec_ctx: &ExecutionContext,
    conn_txn: &mut ConnectionTxn,
    ctx: &mut SessionContext,
) -> Result<u64, DbError> { ... }

// Refactored CSV path inside execute_copy_from:
CopyFormat::Csv => {
    let mut rdr = csv::ReaderBuilder::new()
        .delimiter(delimiter as u8)
        .has_headers(use_header)
        .flexible(false)
        .from_reader(file);
    
    let columns: Option<Vec<String>> = if use_header {
        Some(rdr.headers()?.iter().map(|s| s.trim().to_string()).collect())
    } else {
        None
    };
    
    let mut batch: Vec<Vec<Value>> = Vec::with_capacity(COPY_BATCH_SIZE);
    let mut total: u64 = 0;
    let mut col_count: Option<usize> = None;
    
    for (idx, result) in rdr.records().enumerate() {
        let record = result.map_err(...)?;
        // column count consistency check (no-header mode)
        let row: Vec<Value> = record.iter()
            .map(|f| copy_csv_field_to_value(f, &null_str))
            .collect();
        batch.push(row);
        if batch.len() == COPY_BATCH_SIZE {
            total += flush_batch(&mut batch, columns.clone(), ...)?;
        }
    }
    if !batch.is_empty() {
        total += flush_batch(&mut batch, columns, ...)?;
    }
    total
}
```

### Verification

```bash
limactl shell axiomdb -- bash -c "source ~/.cargo/env && CARGO_TARGET_DIR=\$HOME/axiomdb-target cargo nextest run -p axiomdb-sql --test integration_copy --manifest-path /Users/cristian/nexusdb/.claude/worktrees/beautiful-babbage-88f474/Cargo.toml 2>&1"
```

### Commit

```
feat(fase-20): CSV batch-streaming in COPY FROM (step 1/3)

COPY_BATCH_SIZE=1024; flush_batch helper; CSV loop replaces collect-all.
Tests: empty CSV, exact batch, batch+1.
```

---

## Step 2 — JSONL schema-first streaming

**Goal:** Replace the two-pass JSONL column-discovery loop with a schema-first
single-pass batch loop.

**Files:** `copy_from.rs`, `integration_copy.rs`

### Tests to add

```rust
#[test]
fn test_copy_from_jsonl_unknown_key_ignored() {
    // Row has key "extra" not in schema → inserted with NULLs for that column,
    // unknown key silently dropped
}

#[test]
fn test_copy_from_jsonl_missing_key_is_null() {
    // Row is missing a nullable column → inserted as NULL
}

#[test]
fn test_copy_from_jsonl_error_mid_stream_rolls_back() {
    // Lines 1-3 valid, line 4 is invalid JSON → error returned,
    // rows from lines 1-3 NOT committed (whole COPY is atomic)
}
```

### Implementation outline

```rust
// JSONL path inside execute_copy_from (after resolve_table_cached call):
CopyFormat::Jsonl => {
    // Schema-first: resolve table columns once
    let resolved = resolve_table_cached(
        exec_ctx.storage(), exec_ctx.coord(), ctx,
        Some(conn_txn), &TableRef::simple(stmt.table.clone()),
    )?;
    let col_index: hashbrown::HashMap<String, usize> = resolved.columns
        .iter().enumerate()
        .map(|(i, c)| (c.name.clone(), i))
        .collect();
    let col_count = resolved.columns.len();
    let column_names: Vec<String> = resolved.columns.iter().map(|c| c.name.clone()).collect();
    
    let reader = std::io::BufReader::new(file);
    let mut batch: Vec<Vec<Value>> = Vec::with_capacity(COPY_BATCH_SIZE);
    let mut total: u64 = 0;
    
    for (line_idx, line_result) in reader.lines().enumerate() {
        let line = line_result.map_err(DbError::Io)?;
        let line = line.trim();
        if line.is_empty() { continue; }
        
        let obj: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(line).map_err(|e| DbError::InvalidValue {
                reason: format!("COPY FROM: {}: line {}: JSON parse error: {e}",
                    stmt.path, line_idx + 1),
            })?;
        
        let mut row = vec![Value::Null; col_count];
        for (key, val) in &obj {
            let lc = key.to_ascii_lowercase();
            if let Some(&idx) = col_index.get(&lc) {
                row[idx] = copy_json_to_value(val);
            }
            // unknown keys silently ignored
        }
        batch.push(row);
        if batch.len() == COPY_BATCH_SIZE {
            total += flush_batch(&mut batch, Some(column_names.clone()), ...)?;
        }
    }
    if !batch.is_empty() {
        total += flush_batch(&mut batch, Some(column_names), ...)?;
    }
    total
}
```

### Verification

```bash
limactl shell axiomdb -- bash -c "source ~/.cargo/env && CARGO_TARGET_DIR=\$HOME/axiomdb-target cargo nextest run -p axiomdb-sql --test integration_copy --manifest-path /Users/cristian/nexusdb/.claude/worktrees/beautiful-babbage-88f474/Cargo.toml 2>&1"
```

### Commit

```
feat(fase-20): JSONL schema-first streaming in COPY FROM (step 2/3)

Single-pass batch loop; col_index from table schema; unknown keys ignored;
missing keys → NULL. Tests: unknown key, missing key, mid-stream rollback.
```

---

## Step 3 — Wire smoke + close

**Goal:** Add 2 wire assertions for 20.8, update all docs, close subphase.

**Files:** `tools/wire-test.py`, `docs/progreso.md`, `docs/fase-20.md`,
`docs-site/src/user-guide/sql-reference/ddl.md`, `memory/project_state.md`

### Wire assertions to add

```python
# 20.8a: COPY FROM a 2000-row CSV succeeds (count == 2000)
# 20.8b: COPY FROM a JSONL with unknown/missing keys succeeds (rows with NULLs)
```

### Verification against spec

- [ ] `COPY_BATCH_SIZE = 1024` constant defined
- [ ] CSV streaming: no `Vec<Vec<Value>>` accumulation
- [ ] JSONL: schema-first, single-pass
- [ ] Empty file → `Affected { count: 0 }`
- [ ] Error mid-stream → `Err` returned (rollback by caller)
- [ ] `cargo nextest run -p axiomdb-sql` passes
- [ ] `cargo nextest run --workspace` passes
- [ ] `cargo clippy --workspace -- -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] Wire: 2 new assertions, all passing

### Final commit

```
feat(fase-20): complete 20.8 — COPY FROM streaming (batch loop + JSONL schema-first)

Implements specs/fase-20/spec-copy-streaming.md
Plan: specs/fase-20/plan-copy-streaming.md
Tests: 6 new integration tests
Wire: 564/564 passed
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| `resolve_table_cached` not accessible from inside `execute_copy_from` | low | it's already used by `copy_to.rs` in the same include scope |
| `execute_insert_ctx` side-effects on repeated calls (e.g. auto-increment gaps) | low | same as today — each row already gets its own insert; batching changes nothing |
| JSONL test rollback hard to assert with MemoryStorage | medium | check row count before/after; assert table is empty after failed COPY |

## Rollback plan

1. `git reset --hard` to commit before Step 1 — changes confined to one file.
2. Mark spec status back to `draft`.

## Estimated effort

Total: 2–3 hours
Per step: Step 1: 45 min, Step 2: 45 min, Step 3: 30 min
