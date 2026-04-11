# Plan: 11.17 GIN Index for JSONB

## Current status

Implemented as of 2026-04-11, with two material differences from the original
plan below:

- Clustered tables use encoded primary-key bookmarks in GIN keys, not heap RIDs.
- The current term extraction path supports `JSONB`, `JSON`, and text values
  containing valid JSON. Strict DDL rejection for non-JSON columns remains a
  follow-up acceptance item in `spec-11.17-gin-jsonb.md`.

The remaining sections are the historical implementation plan and are kept for
context.

## Files to create/modify

| File | Action |
|---|---|
| `crates/axiomdb-types/src/jsonb.rs` | ADD `gin_extract_terms()` — DFS term extractor |
| `crates/axiomdb-sql/src/expr.rs` | ADD `BinaryOp::JsonContains` |
| `crates/axiomdb-sql/src/ast.rs` | ADD `IndexType::Gin` |
| `crates/axiomdb-sql/src/parser/expr.rs` | ADD `@>` token → `BinaryOp::JsonContains` |
| `crates/axiomdb-sql/src/parser/ddl.rs` | CHANGE `"gin"` → `IndexType::Gin` (separate from `fts`) |
| `crates/axiomdb-sql/src/eval/ops.rs` | ADD arm for `BinaryOp::JsonContains` |
| `crates/axiomdb-sql/src/executor/ddl_create_index.rs` | ADD `IndexType::Gin → 4` mapping + JSONB column validation |
| `crates/axiomdb-sql/src/index_maintenance.rs` | ADD `index_type == 4` GIN insert + GIN delete |
| `crates/axiomdb-sql/src/planner_types.rs` | ADD `AccessMethod::GinScan { index_def, query_terms }` |
| `crates/axiomdb-sql/src/planner_select.rs` | ADD Rule N: detect `col @> literal` with GIN index |
| `crates/axiomdb-sql/src/executor/select_core.rs` | ADD `GinScan` arm in match |
| `crates/axiomdb-sql/src/executor/select_ctx.rs` | ADD `GinScan` arm in match |
| `crates/axiomdb-sql/src/executor/exec_explain.rs` | ADD `GinScan` arm in EXPLAIN output |
| `crates/axiomdb-sql/tests/integration_jsonb.rs` | ADD GIN index tests (≥15 new test cases) |
| `tools/wire-test.py` | ADD GIN wire smoke |

## Algorithm / Data Structures

### Term encoding (PostgreSQL jsonb_ops compatible)

```rust
// crates/axiomdb-types/src/jsonb.rs
pub const GIN_FLAG_KEY: u8 = 0x01;   // object key or string array element
pub const GIN_FLAG_NULL: u8 = 0x02;  // null value
pub const GIN_FLAG_BOOL: u8 = 0x03;  // bool value (payload: 0/1)
pub const GIN_FLAG_NUM: u8 = 0x04;   // numeric (payload: canonical string)
pub const GIN_FLAG_STR: u8 = 0x05;   // string value (non-key)

/// Extract all GIN terms from a JSONB document (DFS, all levels).
/// Returns a Vec<Vec<u8>> where each element is one [flag][payload] term.
pub fn gin_extract_terms(data: &[u8]) -> Result<Vec<Vec<u8>>, DbError>
```

Term extraction rules (from jsonb_gin.c `gin_extract_jsonb`):
- `WJB_KEY`  → `[GIN_FLAG_KEY][key_bytes]`
- `WJB_ELEM` where element is string → `[GIN_FLAG_KEY][string_bytes]` (PostgreSQL compat)
- `WJB_ELEM` where element is non-string → value term for its type
- `WJB_VALUE` → value term for its type

Value term encoding:
- Null      → `[GIN_FLAG_NULL]` (no payload)
- Bool      → `[GIN_FLAG_BOOL][0u8 or 1u8]`
- Numeric   → `[GIN_FLAG_NUM][canonical_decimal_string_bytes]`
  - Int/BigInt: format as decimal string, e.g., `42` → b"42"
  - Real: format with enough precision, e.g., `3.14` → b"3.14"
- String    → `[GIN_FLAG_STR][string_bytes]`
- Container → recurse (no top-level container term, only its leaves)

### B-Tree key format

```
[term_bytes][0x00 separator][page_id: 8 LE][slot_id: 2 LE]
```

This allows a range scan `[term][0x00][0x00..00]` to `[term][0x00][0xFF..FF]`
to collect all RIDs for a given term in a single B-Tree range call.

### GIN INSERT (index_maintenance.rs)

```rust
if idx.index_type == 4 {
    let col_idx = idx.columns[0].col_idx as usize;
    let jsonb_bytes = match row.get(col_idx) {
        Some(Value::Jsonb(b)) => b.clone(),
        Some(Value::Json(s)) => Arc::new(JsonbEncoder::encode(&serde_json::from_str(s)?)?),
        _ => continue,
    };
    let terms = gin_extract_terms(&jsonb_bytes)?;
    let root_pid = AtomicU64::new(idx.root_page_id);
    for term in &terms {
        let mut key = term.clone();
        key.push(0x00);
        key.extend_from_slice(&rid.page_id.to_le_bytes());
        key.extend_from_slice(&rid.slot_id.to_le_bytes());
        BTree::insert_in(storage, &root_pid, &key, rid, idx.fillfactor);
    }
    // persist new root if split occurred
    continue;
}
```

### GIN DELETE

Same term extraction, then `BTree::delete_in` for each term key.
Must be added to `delete_many_from_single_index` and the inline delete paths.

### Planner rule (planner_select.rs)

```
Rule GIN: detect Expr::BinaryOp { op: JsonContains, left: Expr::Column(col), right: Expr::Literal(Value::Json|Jsonb) }
  1. Find GIN index (index_type == 4) on col
  2. Decode query doc from literal
  3. Extract query terms via gin_extract_terms()
  4. Return AccessMethod::GinScan { index_def, query_terms: Vec<Vec<u8>> }
```

### GIN executor (select_core.rs + select_ctx.rs)

```rust
AccessMethod::GinScan { index_def, query_terms } => {
    // 1. For each query term, range-scan the B-Tree for matching RIDs
    let mut candidate_sets: Vec<HashSet<RecordId>> = Vec::new();
    for term in &query_terms {
        let lo = { let mut k = term.clone(); k.push(0x00); k.extend(repeat(0x00).take(10)); k };
        let hi = { let mut k = term.clone(); k.push(0x00); k.extend(repeat(0xFF).take(10)); k };
        let pairs = BTree::range_in(storage, index_def.root_page_id, Some(&lo), Some(&hi))?;
        candidate_sets.push(pairs.into_iter().map(|(rid, _)| rid).collect());
    }
    // 2. Intersect all candidate sets (rows that match ALL query terms)
    let candidates: HashSet<RecordId> = if candidate_sets.is_empty() {
        HashSet::new()
    } else {
        candidate_sets.into_iter().reduce(|a, b| a.intersection(&b).copied().collect())
            .unwrap_or_default()
    };
    // 3. Load rows + visibility check + verify with jsonb_contains (no false positives)
    let mut result = vec![];
    for rid in candidates {
        if !HeapChain::is_slot_visible(storage, rid.page_id, rid.slot_id, snap.clone())? {
            continue;
        }
        if let Some(values) = TableEngine::read_row(storage, &resolved.columns, rid)? {
            result.push((rid, values));
        }
    }
    result
}
```

### @> operator

```rust
// expr.rs: BinaryOp::JsonContains
// eval/ops.rs:
BinaryOp::JsonContains => {
    let (l, r) = (eval(left, row)?, eval(right, row)?);
    match (&l, &r) {
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (Value::Jsonb(doc), Value::Jsonb(q)) => Ok(Value::Int(jsonb_contains(doc, q)? as i32)),
        (Value::Json(doc), Value::Json(q)) | ... => {
            // encode to JSONB then call jsonb_contains
        }
        // cross-type: encode both sides and check
    }
}
```

## Implementation phases

1. **Term extractor** (`jsonb.rs`) — `gin_extract_terms()`, `GIN_FLAG_*` constants. Unit tests.
2. **`@>` operator** — `BinaryOp::JsonContains`, parser, evaluator. Tests without index.
3. **`IndexType::Gin`** — AST, parser, DDL mapping to `index_type=4`. `CREATE INDEX USING gin` validation.
4. **GIN INSERT maintenance** — `index_maintenance.rs` arm for `index_type==4`. Integration test: insert + re-read index.
5. **GIN DELETE maintenance** — delete paths. Update paths (delete old + insert new).
6. **`AccessMethod::GinScan`** — planner rule + planner type. Unit test: planner detects `col @> literal`.
7. **GIN executor** — `select_core.rs` + `select_ctx.rs`. Integration tests: queries return correct rows.
8. **EXPLAIN output** — `exec_explain.rs` for `GinScan`.
9. **Wire smoke** — wire-test.py JSONB GIN section.
10. **Closing** — workspace clean, clippy, fmt, docs, commit.

## Tests to write (≥15 new cases in integration_jsonb.rs)

```
test_gin_index_create_on_jsonb          — CREATE succeeds
test_gin_index_create_on_non_jsonb      — CREATE on INT column → error
test_gin_json_contains_no_index         — @> correct without index (full scan)
test_gin_json_contains_simple           — {a:1} @> {a:1} with index → match
test_gin_json_contains_subset           — {a:1,b:2} @> {a:1} → match
test_gin_json_contains_mismatch         — {a:1} @> {a:2} → no match
test_gin_json_contains_nested           — {a:{b:1}} @> {a:{b:1}} → match
test_gin_json_contains_array            — [1,2,3] @> [2] → match
test_gin_json_contains_empty_query      — doc @> {} → always match
test_gin_index_insert_delete            — insert + delete, query returns empty
test_gin_index_update                   — update JSONB value, query reflects change
test_gin_multi_term_and                 — {a:1,b:2} @> {a:1,b:2} → both terms must match
test_gin_null_operand                   — NULL @> {a:1} → NULL
test_gin_planner_uses_index             — verify AccessMethod::GinScan chosen
test_gin_large_dataset                  — 1000 rows, GIN lookup faster than scan (not a perf test, just correctness at scale)
```

## Anti-patterns to avoid

- **No false negatives**: GIN term intersection gives candidates; always verify with `jsonb_contains()` for correctness. Never skip the recheck.
- **No spin on root PID**: use `AtomicU64::new(idx.root_page_id)` + `load(Acquire)` pattern, same as Trigram/FTS arms.
- **Intersection before heap read**: intersect RID sets in memory before loading heap rows; don't read the heap for each term separately.
- **GIN on non-JSONB column**: `ddl_create_index.rs` must validate column type; return `DbError::InvalidOperation` with clear message.
- **`@>` fallback**: if no GIN index, evaluator handles `@>` via `jsonb_contains()` — planner returns `Scan`, WHERE filter evaluates the operator.

## Risks

- **R1**: `index_type=4` must not conflict with future index types. Current: 0=BTree, 1=Brin, 2=Trigram, 3=FTS, 4=GIN. Reserve 5+ for future.
- **R2**: GIN delete is expensive (one B-Tree delete per term). Acceptable for Phase 11; GIN fast-update pending list is a Phase 30 optimization.
- **R3**: Empty query (`@> '{}'`) must return all rows — not zero rows. Handle by returning `AccessMethod::Scan` from planner when query_terms is empty.
- **R4**: Cross-type `@>` (e.g., `JSON @> JSONB`): encode both sides to JSONB before calling `jsonb_contains()`.
