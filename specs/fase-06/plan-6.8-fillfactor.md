# Plan: Fill Factor (Phase 6.8)

## Files to create / modify

### Create
- `crates/axiomdb-sql/tests/integration_fillfactor.rs` — integration tests

### Modify
- `crates/axiomdb-catalog/src/schema.rs` — `IndexDef.fillfactor: u8` + serde
- `crates/axiomdb-sql/src/ast.rs` — `CreateIndexStmt.fillfactor: Option<u8>`
- `crates/axiomdb-sql/src/parser/ddl.rs` — parse `WITH (fillfactor=N)`
- `crates/axiomdb-index/src/tree.rs` — `fill_threshold`, thread `fillfactor` through insert
- `crates/axiomdb-sql/src/index_maintenance.rs` — pass `idx.fillfactor` to `insert_in`
- `crates/axiomdb-sql/src/executor.rs` — pass fillfactor at all `BTree::insert_in` call sites

---

## Algorithm

### `fill_threshold(order: usize, fillfactor: u8) -> usize`

```rust
/// Returns the maximum number of keys a leaf page may hold before splitting.
///
/// Uses ceiling division so fillfactor=100 gives exactly ORDER_LEAF (current behavior).
///
/// Examples (ORDER_LEAF = 217):
///   fillfactor=100 → 217  (no change from today)
///   fillfactor=90  → 196  (default)
///   fillfactor=70  → 152
///   fillfactor=10  →  22  (minimum meaningful)
pub fn fill_threshold(order: usize, fillfactor: u8) -> usize {
    let ff = fillfactor as usize;
    ((order * ff + 99) / 100).max(1)
}
```

**Invariant:** `fill_threshold(ORDER_LEAF, 100) == ORDER_LEAF`. Verified with a const-assert.

### `IndexDef` on-disk extension

Append 1 byte after the predicate section (backward-compatible):

```text
[...existing fields...][ncols:1][columns...]
[pred_len:2][pred_sql bytes]    ← Phase 6.7
[fillfactor: 1 byte]            ← Phase 6.8 NEW; absent → default 90
```

`to_bytes()`: always write the fillfactor byte (even for fillfactor=90).
`from_bytes()`: after reading predicate, if `bytes.len() > consumed` → read 1 byte as fillfactor; else → `fillfactor = 90`.

### `BTree::insert_in` — new signature

```rust
pub fn insert_in(
    storage: &mut dyn StorageEngine,
    root_pid: &AtomicU64,
    key: &[u8],
    rid: RecordId,
    fillfactor: u8,   // 10–100; 100 = current behavior
) -> Result<(), DbError>
```

Thread `fillfactor` through:
- `insert_subtree(storage, pid, key, rid, fillfactor: u8)`
- `insert_leaf(storage, old_pid, node, key, rid, fillfactor: u8)`

### `insert_leaf` change — split threshold

```rust
// Before:
if node.num_keys() < ORDER_LEAF { ... in-place ... }

// After:
let threshold = fill_threshold(ORDER_LEAF, fillfactor);
if node.num_keys() < threshold { ... in-place ... }
```

**Internal node split threshold is unchanged**: `if n < ORDER_INTERNAL { ... }`.
Internal nodes always fill to capacity (same as PostgreSQL's fixed
`BTREE_NONLEAF_FILLFACTOR = 70` — not user-configurable).

---

## Implementation phases

### Phase 1 — Catalog + AST + Parser

**Step 1.1** — `schema.rs`:
Add `fillfactor: u8` to `IndexDef` with `#[doc]` noting the 10–100 range and
default of 90. Update `to_bytes()` to append the byte. Update `from_bytes()` to
read it if bytes remain after predicate; otherwise default to 90. Update all
`IndexDef { ... }` literals in tests to include `fillfactor: 90`.

**Step 1.2** — `ast.rs`:
Add `fillfactor: Option<u8>` to `CreateIndexStmt`. `None` → use default 90 at
persist time.

**Step 1.3** — `parser/ddl.rs` in `parse_create_index`:
After the `[WHERE pred]` clause (already parsed in 6.7), add:

```rust
// Parse optional WITH (key = value, ...)
let fillfactor: Option<u8> = if p.eat(&Token::With) {
    p.expect(&Token::LParen)?;
    let mut ff: Option<u8> = None;
    loop {
        let key = p.parse_identifier()?;
        p.expect(&Token::Eq)?;
        match key.to_lowercase().as_str() {
            "fillfactor" => {
                let val = p.parse_integer_literal()?;  // returns i64
                if !(10..=100).contains(&val) {
                    return Err(DbError::ParseError {
                        message: "fillfactor must be between 10 and 100".into(),
                    });
                }
                ff = Some(val as u8);
            }
            other => return Err(DbError::ParseError {
                message: format!("unknown index option: {other}"),
            }),
        }
        if !p.eat(&Token::Comma) { break; }
    }
    p.expect(&Token::RParen)?;
    ff
} else {
    None
};
```

**Note:** `parse_integer_literal()` needs to be added to the parser (reads a
`Token::Integer` and returns `i64`). Alternatively, `parse_expr` + pattern-match
on `Expr::Literal(Value::Int(n))`.

**Step 1.4** — `executor.rs` in `execute_create_index`:
When persisting `IndexDef`, use `stmt.fillfactor.unwrap_or(90)` as the fillfactor.

**Verify:** `cargo build --workspace` clean. No test regressions.

---

### Phase 2 — B-Tree: thread fillfactor through insert

**Step 2.1** — `tree.rs`: Add `fill_threshold` function (free function in the
module, not a method). Add const-assert:

```rust
const _: () = assert!(
    fill_threshold_const(ORDER_LEAF, 100) == ORDER_LEAF,
    "fill_threshold(100) must equal ORDER_LEAF"
);
```

Since `fill_threshold` uses arithmetic, not loops, it can be a `const fn`.

**Step 2.2** — `tree.rs`: Update `insert_in` signature to add `fillfactor: u8`.
Pass it to `insert_subtree`.

**Step 2.3** — `tree.rs`: Update `insert_subtree` signature to add `fillfactor: u8`.
Pass it to recursive `insert_subtree` calls and to `insert_leaf`.
Internal node split (`split_internal`) is **not** affected — keep as-is.

**Step 2.4** — `tree.rs`: Update `insert_leaf` signature to add `fillfactor: u8`.
Change the in-place threshold from `ORDER_LEAF` to `fill_threshold(ORDER_LEAF, fillfactor)`.
The split itself (mid calculation, page allocation) is **unchanged** — keep 50-50.

**Verify:** `cargo build -p axiomdb-index` clean. B-Tree unit tests pass.

---

### Phase 3 — Wire fillfactor at all call sites

**Step 3.1** — `index_maintenance.rs` in `insert_into_indexes`:
Change `BTree::insert_in(storage, &root_pid, &key, rid)?`
to `BTree::insert_in(storage, &root_pid, &key, rid, idx.fillfactor)?`.

**Step 3.2** — `executor.rs` line 4080 (`execute_create_index` build loop):
Change to `BTree::insert_in(storage, &root_pid, &key, rid, fillfactor)?`
where `fillfactor = stmt.fillfactor.unwrap_or(90)` (computed before the loop).

**Step 3.3** — `executor.rs` in `execute_create_table` (PK/UNIQUE index creation,
the `create_empty_index` inline helper): those indexes start empty (no rows to
insert during CREATE TABLE), so `BTree::insert_in` is not called there. ✅ No change.

**Step 3.4** — `executor.rs` in `persist_fk_constraint` (FK auto-index creation):
The FK auto-index also iterates existing rows and calls `BTree::insert_in`. Use
`fillfactor = 90` (default — FK auto-indexes have no user-configurable fillfactor).

**Verify:** `cargo build --workspace` clean. `cargo test --workspace` 1246+ passing.

---

### Phase 4 — Tests

**Integration tests** (`tests/integration_fillfactor.rs`):

```rust
// DDL: fillfactor persisted
fn test_create_index_with_fillfactor_persists()
fn test_create_index_default_fillfactor_is_90()
fn test_create_index_without_with_clause_default_90()

// Validation
fn test_fillfactor_below_10_rejected()
fn test_fillfactor_above_100_rejected()
fn test_fillfactor_exactly_10_accepted()
fn test_fillfactor_exactly_100_accepted()
fn test_unknown_with_option_rejected()

// Behavioral: fillfactor=100 is identical to current behavior
fn test_fillfactor_100_no_regression()

// Behavioral: fillfactor=70 keeps pages less dense
fn test_fillfactor_70_pages_less_dense()
// Strategy: insert ORDER_LEAF+1 rows → with ff=70 should cause
// an early split at 152 entries instead of at 217.
// Verify by checking the B-Tree depth or measuring page count
// via the index root page structure.

// Backward compat: pre-6.8 IndexDef row reads as fillfactor=90
fn test_pre_68_index_row_reads_as_default_90()

// Schema roundtrip
fn test_index_def_fillfactor_roundtrip()
```

**Unit tests** (in `tree.rs`):

```rust
fn test_fill_threshold_100() { assert_eq!(fill_threshold(217, 100), 217); }
fn test_fill_threshold_90()  { assert_eq!(fill_threshold(217, 90), 196); }
fn test_fill_threshold_70()  { assert_eq!(fill_threshold(217, 70), 152); }
fn test_fill_threshold_10()  { assert_eq!(fill_threshold(217, 10), 22); }
fn test_fill_threshold_min_1() { assert_eq!(fill_threshold(1, 10), 1); }
```

---

## Anti-patterns to avoid

- **DO NOT** change the internal node split threshold — it must stay at
  `ORDER_INTERNAL` regardless of fillfactor. The comment must say why.
- **DO NOT** check `fillfactor` in `split_internal` — only `insert_leaf` uses it.
- **DO NOT** use integer division for the threshold — `(order * ff) / 100` would
  give `(217 * 100) / 100 = 217` ✅ but `(217 * 90) / 100 = 195` (rounds down).
  Use **ceiling division**: `(order * ff + 99) / 100` → `(217 * 90 + 99) / 100 = 196`.
  The difference matters: floor(90%) = 195, ceil(90%) = 196.
- **DO NOT** use `0` as a sentinel for "default" in storage — store the actual
  value 90 explicitly. Using 0-means-90 would break `fillfactor = 0` validation
  (already invalid, but cleaner to store the real value).
- **DO NOT** pass fillfactor through `delete_in` or `delete_leaf` — deletion
  never needs to know fillfactor (only insertion uses the split threshold).

## Risks

| Risk | Mitigation |
|------|-----------|
| `fill_threshold(ORDER_LEAF, 100) != ORDER_LEAF` regression | Const-assert at compile time |
| Integer overflow in `order * ff` | `ORDER_LEAF = 217`, max `ff = 100` → `217 * 100 = 21700` → fits in usize on any platform |
| Existing pages with > threshold entries (pre-6.8) | First insert into such a page triggers split — correct, safe behavior |
| All callers of `insert_in` updated | Compiler catches missing args after signature change |
