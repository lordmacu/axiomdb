# Plan: 4.23 — QueryResult type

## Files to create/modify

| File | Action | What it does |
|---|---|---|
| `crates/nexusdb-sql/src/result.rs` | **create** | `Row`, `ColumnMeta`, `QueryResult` + impls + tests |
| `crates/nexusdb-sql/src/lib.rs` | modify | `pub mod result` + re-exports |

No new crate. No new dependency. Two files total.

---

## Algorithm / Data structure

No algorithm — pure type definition. The only non-trivial decisions are already
settled in the spec:

- `Row = Vec<Value>` type alias (not newtype — avoids boilerplate `Deref` impls)
- `ColumnMeta` has four fields; derives `Debug + Clone + PartialEq`
- `QueryResult` has three variants; derives `Debug + Clone + PartialEq`
- Convenience constructors on both types to reduce verbosity in the executor

### Complete module layout

```
result.rs
  ├── pub type Row = Vec<Value>;
  ├── pub struct ColumnMeta { name, data_type, nullable, table_name }
  │     └── impl ColumnMeta { new() }
  ├── pub enum QueryResult { Rows{..}, Affected{..}, Empty }
  │     └── impl QueryResult {
  │           empty_rows(), affected(), affected_with_id()
  │           row_count() -> Option<usize>   // convenience for tests
  │           column_count() -> Option<usize>
  │         }
  └── #[cfg(test)] mod tests { ... }
```

---

## Implementation phases

1. **Create `result.rs`** with type definitions, derives, and doc comments.
   Verify `cargo check -p nexusdb-sql`.

2. **Add `impl ColumnMeta`** — single constructor `new(name, data_type, nullable,
   table_name)`. Verify compiles.

3. **Add `impl QueryResult`** — three convenience constructors and two accessor
   helpers (`row_count`, `column_count`).

4. **Write unit tests** in `result.rs`. See test list below.

5. **Export from `lib.rs`**: add `pub mod result;` and re-export the three public
   types + the `Row` alias.

6. **Run `cargo test --workspace`**, `cargo clippy`, `cargo fmt --check`.

---

## Tests to write

All unit tests live in `result.rs` — no integration tests needed for a pure
data type.

```
// ColumnMeta construction
test_column_meta_new                — fields stored correctly, table_name = None for computed
test_column_meta_clone_eq           — Clone produces identical value; PartialEq holds

// QueryResult::Rows
test_rows_empty                     — empty_rows() has 0 rows, correct columns
test_rows_with_data                 — 2 columns × 3 rows; column_count = 2; row_count = 3
test_rows_clone_eq                  — Clone + PartialEq on Rows variant

// QueryResult::Affected
test_affected_no_id                 — affected(5): count=5, last_insert_id=None
test_affected_with_id               — affected_with_id(1, 42): count=1, last_insert_id=Some(42)
test_affected_zero_rows             — affected(0): count=0

// QueryResult::Empty
test_empty_variant                  — Empty is Empty; row_count = None; column_count = None

// Accessors
test_row_count_rows                 — Rows with 3 rows → row_count() = Some(3)
test_row_count_affected             — Affected → row_count() = None
test_row_count_empty                — Empty → row_count() = None
test_column_count_rows              — Rows with 2 cols → column_count() = Some(2)
test_column_count_non_rows          — Affected / Empty → column_count() = None

// Debug output (smoke test — just ensure it doesn't panic)
test_debug_all_variants             — format!("{:?}", variant) does not panic for each
```

---

## Anti-patterns to avoid

- **DO NOT** make `Row` a newtype (`struct Row(Vec<Value>)`). A type alias is
  sufficient — newtypes require `Deref` / `IntoIterator` impls that add noise
  with no benefit at this stage.
- **DO NOT** add `Display` or ASCII table formatting here. That belongs in the CLI
  (Phase 4.15). `QueryResult` is a data type, not a presentation layer.
- **DO NOT** add a `Error` variant to `QueryResult`. Errors are `Err(DbError)`;
  mixing them into the result type would force every caller to double-unwrap.
- **DO NOT** add `last_insert_id: u64` (non-optional). It must be `Option<u64>` —
  UPDATE, DELETE, and INSERT into tables without AUTO_INCREMENT produce no ID.

---

## Risks

None significant. This is a pure data type with no I/O, no unsafe, and no
complex invariants. The only risk is getting the field names or derives wrong,
which the compiler will catch immediately.
