# Plan: 4.G1 — Trivial Bug Fixes

## Files to modify

| File | Change |
|---|---|
| `crates/axiomdb-sql/src/eval/ops.rs` | `real_arith()` — add zero check before float div |
| `crates/axiomdb-sql/src/eval/functions/string.rs` | `concat()` — NULL propagation; `substr()` — negative index |
| `crates/axiomdb-sql/src/eval/functions/numeric.rs` | `round()` — round-half-up |
| `crates/axiomdb-sql/src/executor/update.rs` | `execute_update_with_candidates()` — call CHECK |
| `crates/axiomdb-sql/src/executor/insert.rs` | column count mismatch; AUTO_INC=0 |
| `crates/axiomdb-sql/src/executor/select.rs` | SELECT * without FROM |
| `crates/axiomdb-core/src/error.rs` OR `crates/axiomdb-sql/src/error.rs` | new `ColumnCountMismatch` variant |

---

## Implementation steps (in order)

### Step 1 — `DbError::ColumnCountMismatch` (new variant)

Find the `DbError` enum (likely in `crates/axiomdb-core/src/error.rs` or `crates/axiomdb-sql/src/error.rs`).
Add:
```rust
#[error("column count doesn't match value count at row {row}: expected {expected}, got {got}")]
ColumnCountMismatch { expected: usize, got: usize, row: usize },
```
Add SQLSTATE mapping: `"21S01"` (MySQL 1136).

---

### Step 2 — `ops.rs`: float division by zero → NULL

Locate `real_arith()` in `eval/ops.rs`. In the `Div` arm:
```rust
// BEFORE
BinaryOp::Div => f64_result(l / r),

// AFTER
BinaryOp::Div => {
    if r == 0.0 {
        return Ok(Value::Null);
    }
    f64_result(l / r)
}
```

---

### Step 3 — `string.rs`: CONCAT(NULL) propagates NULL

Locate `concat()`. In the arm that handles `Value::Null`:
```rust
// BEFORE
Value::Null => {}   // skip NULLs

// AFTER
Value::Null => return Ok(Value::Null),   // any NULL → result NULL
```
Note: `concat_ws()` intentionally skips NULLs — leave it unchanged.

---

### Step 4 — `string.rs`: SUBSTR negative index

Locate `substr()` / `substring()`. After extracting the start index as `i64`:
```rust
// Current (broken for negatives):
let start = (pos as usize).saturating_sub(1);

// Fixed:
let start = if pos < 0 {
    chars.len().saturating_sub(pos.unsigned_abs() as usize)
} else {
    (pos as usize).saturating_sub(1)   // 1-based → 0-based
};
```

---

### Step 5 — `numeric.rs`: ROUND uses round-half-up

Locate `round()`. Replace the core rounding formula:
```rust
// BEFORE (banker's rounding via Rust .round()):
(f * factor).round() / factor

// AFTER (round half away from zero — MySQL behavior):
let shifted = f * factor;
let rounded = if shifted >= 0.0 {
    shifted.floor() + if shifted.fract() >= 0.5 { 1.0 } else { 0.0 }
} else {
    shifted.ceil() - if (-shifted).fract() >= 0.5 { 1.0 } else { 0.0 }
};
rounded / factor
```

---

### Step 6 — `update.rs`: CHECK constraints enforced on UPDATE

In `execute_update_with_candidates()`, before the call to `update_rows_preserve_rid`:

```rust
// Load constraints (same pattern as insert.rs)
let constraints = ctx.catalog_reader().read_constraints(table_def.table_id)?;
if !constraints.is_empty() {
    for new_values in &candidate_new_values {
        check_row_constraints(&constraints, new_values, &table_name)?;
    }
}
```

Import `check_row_constraints` from `insert.rs` by making it `pub(crate)` or moving it to `shared.rs`.

---

### Step 7 — `insert.rs`: column count mismatch

In `execute_insert_rows()` (or wherever the column map is validated), after building
the full `col_map` (column index → value mapping) for a VALUES row:

```rust
if values.len() != col_map.len() {
    return Err(DbError::ColumnCountMismatch {
        expected: col_map.len(),
        got: values.len(),
        row: row_index + 1,
    });
}
```
This check fires after computing which columns are explicit. Row index (1-based) matches MySQL's error message format.

---

### Step 8 — `insert.rs`: AUTO_INCREMENT=0 → next value

In `resolve_auto_increment()`, extend the existing NULL check:
```rust
// BEFORE: only NULL triggers sequence
let is_auto_val = matches!(values[ai_col], Value::Null);

// AFTER: 0 also triggers sequence (MySQL compat)
let is_auto_val = matches!(
    values[ai_col],
    Value::Null | Value::Int(0) | Value::BigInt(0)
);
```

---

### Step 9 — `select.rs`: SELECT * without FROM → empty result

Locate the code around line 783 that returns `NotImplemented` for wildcard without FROM.
Replace with:
```rust
// SELECT * with no FROM → 0-column result (MySQL: SELECT * FROM dual with no table)
return Ok(QueryResult::Rows {
    columns: vec![],
    rows: vec![],
});
```

---

## Tests to write

**File**: `crates/axiomdb-sql/tests/integration_table.rs` (or add a new
`tests/integration_trivial_fixes.rs`)

```
// 4.17e
assert_eq!(exec("SELECT 1.0 / 0"), [[NULL]])
assert_eq!(exec("SELECT -1.0 / 0"), [[NULL]])
assert_eq!(exec("SELECT 5.0 / 2.0"), [[2.5]])   // normal still works

// 4.19e
assert_eq!(exec("SELECT CONCAT(NULL, 'a')"), [[NULL]])
assert_eq!(exec("SELECT CONCAT('a', NULL, 'b')"), [[NULL]])
assert_eq!(exec("SELECT CONCAT('a', 'b')"), [["ab"]])
assert_eq!(exec("SELECT CONCAT_WS(',', NULL, 'a')"), [["a"]])  // CONCAT_WS unchanged

// 4.19f
assert_eq!(exec("SELECT SUBSTR('hello', -3)"), [["llo"]])
assert_eq!(exec("SELECT SUBSTR('hello', -1)"), [["o"]])
assert_eq!(exec("SELECT SUBSTR('hello', -10)"), [["hello"]])   // clamp
assert_eq!(exec("SELECT SUBSTR('hello', 2)"), [["ello"]])      // positive unchanged

// 4.19k
assert_eq!(exec("SELECT ROUND(2.5)"), [[3]])
assert_eq!(exec("SELECT ROUND(3.5)"), [[4]])
assert_eq!(exec("SELECT ROUND(-2.5)"), [[-3]])
assert_eq!(exec("SELECT ROUND(2.45, 1)"), [[2.5]])

// 4.25d
CREATE TABLE products (id INT PRIMARY KEY, price DECIMAL(10,2), CHECK (price > 0));
INSERT INTO products VALUES (1, 10.00);
UPDATE products SET price = -1 WHERE id = 1;  // → CheckViolation error
UPDATE products SET price = 20.00 WHERE id = 1;  // → OK

// 4.6d
CREATE TABLE t2 (a INT, b INT);
INSERT INTO t2 (a, b) VALUES (1);  // → error 1136

// 4.5c
SELECT *;  // → empty result, 0 rows, 0 columns

// 4.5f
CREATE TABLE seq_test (id INT AUTO_INCREMENT PRIMARY KEY, val TEXT);
INSERT INTO seq_test (id, val) VALUES (0, 'x');  // → id = 1 (not 0)
INSERT INTO seq_test (id, val) VALUES (0, 'y');  // → id = 2
SELECT id FROM seq_test;  // → [1, 2]
```

## Anti-patterns to avoid

- DO NOT change `concat_ws()` — it intentionally skips NULLs by SQL standard
- DO NOT change integer division by zero (already errors correctly)
- DO NOT change ROUND for DECIMAL type — different precision model
- DO NOT call `cargo test --workspace` during implementation; use
  `cargo test -p axiomdb-sql` only

## Risks

- ROUND formula change: verify edge cases `ROUND(0.5)` → 1, `ROUND(-0.5)` → -1
  before committing; IEEE vs MySQL diverge here
- CHECK on UPDATE: `read_constraints()` may need `ctx` to have the reader already
  initialized; check that catalog read path is identical to how insert.rs does it
- AUTO_INC=0: only applies when the column has `auto_increment` flag set — do not
  affect tables where 0 is a valid non-AI value
