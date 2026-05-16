# Plan: Integer Types Completeness

Phase: 24 — Complete Types
Task: 24.1 — TINYINT fix, SMALLINT wire type, BIGSERIAL
Spec: specs/fase-24/spec-24.1-integer-types.md
Status: in-progress

## Summary

Five steps, bottom-up: (1) add DataType variants in axiomdb-types; (2) add ColumnType
variants in axiomdb-catalog; (3) wire every match arm that dispatches on DataType/ColumnType
(table.rs conversions, result.rs wire codes, ddl_show.rs type names, ddl.rs DataType→ColumnType);
(4) fix the parser (TINYINT→TinyInt, SMALLINT→SmallInt, add BIGSERIAL); (5) add range-check
in coerce_values + integration tests + wire smoke. This order ensures the codebase compiles
at each step — new variants are added before the match arms that consume them.

## Dependencies

Must be done first:
- [x] spec-24.1-integer-types approved

Blocks:
- [ ] 24.14b MySQL type aliases (MEDIUMINT, TINYINT UNSIGNED, etc.)

## Affected files

Modified files:
- `crates/axiomdb-types/src/types.rs` — add TinyInt, SmallInt to DataType enum + Display + name()
- `crates/axiomdb-catalog/src/schema_table.rs` — add ColumnType::TinyInt=19, SmallInt=20
- `crates/axiomdb-catalog/src/schema.rs` — update roundtrip test + invalid discriminant test
- `crates/axiomdb-sql/src/table.rs` — update column_type_to_data_type, column_data_types, coerce_values, coerce_values_with_ctx
- `crates/axiomdb-sql/src/parser/ddl.rs` — fix parser (TINYINT→TinyInt, SMALLINT→SmallInt, BIGSERIAL), update fdw_datatype_to_column_type, update data_type_to_column_type match
- `crates/axiomdb-sql/src/executor/ddl_show.rs` — update column_type_to_sql_name + scalar_type_to_sql_name
- `crates/axiomdb-network/src/mysql/result.rs` — update datatype_to_mysql_type, column_display_len, column_flags, value serialization arm
- `crates/axiomdb-sql/src/catalog/array_codec.rs` or `axiomdb-types/src/array_codec.rs` — data_type_to_column_type (if it exists there)

New files:
- `crates/axiomdb-sql/tests/integration_integer_types.rs` — integration tests

---

## Step 1 — Add DataType::TinyInt and DataType::SmallInt

**Goal:** Add two new DataType variants so downstream match arms can reference them.
**Files:** `crates/axiomdb-types/src/types.rs`

### Implementation outline

Add after `DataType::Int`:
```rust
/// SQL TINYINT — i8 range (-128..=127). Stored as i64 at runtime (Value::Int).
/// Distinct from Bool: TINYINT is a numeric type, not boolean.
TinyInt,
/// SQL SMALLINT — i16 range (-32768..=32767). Stored as i64 at runtime (Value::Int).
SmallInt,
```

Update `impl DataType { fn name(&self) -> &'static str }` and `impl Display for DataType`:
```rust
DataType::TinyInt => "TINYINT",
DataType::SmallInt => "SMALLINT",
```

### Verification
```bash
limactl shell axiomdb -- bash -c "source ~/.cargo/env && CARGO_TARGET_DIR=\$HOME/axiomdb-target cargo nextest run -p axiomdb-types 2>&1 | tail -5"
```

### Commit
```
feat(fase-24): add DataType::TinyInt and DataType::SmallInt variants

Step 1 of specs/fase-24/plan-24.1-integer-types.md
```

---

## Step 2 — Add ColumnType::TinyInt=19 and ColumnType::SmallInt=20

**Goal:** Extend the on-disk type tag enum with tags 19 and 20.
**Files:** `crates/axiomdb-catalog/src/schema_table.rs`, `crates/axiomdb-catalog/src/schema.rs`

### Implementation outline

In the ColumnType enum (schema_table.rs), add after Xml=18:
```rust
TinyInt = 19,
SmallInt = 20,
```

Update the `TryFrom<u8>` impl to handle 19 → TinyInt, 20 → SmallInt.

Update `From<ColumnType> for u8` (or the into() impl) — if it's derived, nothing to do;
if it's manual, add the two new arms.

In `schema.rs` tests:
- Add TinyInt and SmallInt to `test_column_type_roundtrip_all_variants`
- Update the comment `// Discriminants 0, 19-254, and 255 are invalid` → `// 0, 21-254, 255`
- Update `test_column_type_invalid_discriminant` to check 21 instead of 19

### Verification
```bash
limactl shell axiomdb -- bash -c "source ~/.cargo/env && CARGO_TARGET_DIR=\$HOME/axiomdb-target cargo nextest run -p axiomdb-catalog 2>&1 | tail -5"
```

### Commit
```
feat(fase-24): add ColumnType::TinyInt=19 and SmallInt=20

Step 2 of specs/fase-24/plan-24.1-integer-types.md
```

---

## Step 3 — Wire DataType↔ColumnType conversions across the codebase

**Goal:** All match-on-DataType and match-on-ColumnType expressions compile and handle TinyInt/SmallInt.
**Files:** `table.rs`, `ddl.rs` (fdw_datatype_to_column_type + data_type_to_column_type), `ddl_show.rs`, `result.rs`, `array_codec.rs` if needed.

### Implementation outline

**`crates/axiomdb-sql/src/table.rs` — column_type_to_data_type:**
```rust
ColumnType::TinyInt => DataType::TinyInt,
ColumnType::SmallInt => DataType::SmallInt,
```

**`crates/axiomdb-sql/src/table.rs` — column_data_types:**
Same two arms in the inline match.

**`crates/axiomdb-sql/src/table.rs` — coerce_values:**
```rust
ColumnType::TinyInt => DataType::TinyInt,
ColumnType::SmallInt => DataType::SmallInt,
```

**`crates/axiomdb-sql/src/table.rs` — coerce_values_with_ctx:**
Same two arms.

**`crates/axiomdb-sql/src/parser/ddl.rs` — data_type_to_column_type (line ~2148):**
```rust
DataType::TinyInt => Ok(ColumnType::TinyInt),
DataType::SmallInt => Ok(ColumnType::SmallInt),
```

**`crates/axiomdb-sql/src/parser/ddl.rs` — fdw_datatype_to_column_type:**
```rust
DataType::TinyInt => Ok(ColumnType::TinyInt),
DataType::SmallInt => Ok(ColumnType::SmallInt),
```

**`crates/axiomdb-sql/src/executor/ddl_show.rs` — column_type_to_sql_name:**
```rust
ColumnType::TinyInt => "TINYINT",
ColumnType::SmallInt => "SMALLINT",
```
And `scalar_type_to_sql_name` if it exists separately.

**`crates/axiomdb-network/src/mysql/result.rs` — datatype_to_mysql_type:**
```rust
DataType::TinyInt  => 0x01,  // TINY
DataType::SmallInt => 0x02,  // SHORT
```

**`crates/axiomdb-network/src/mysql/result.rs` — column_display_len:**
```rust
DataType::TinyInt  => 4,   // "-128" max 4 chars
DataType::SmallInt => 6,   // "-32768" max 6 chars
```

**`crates/axiomdb-network/src/mysql/result.rs` — column_flags:**
```rust
DataType::TinyInt  => 0x0000,  // SIGNED
DataType::SmallInt => 0x0000,  // SIGNED
```

**`crates/axiomdb-network/src/mysql/result.rs` — value serialization:**
TinyInt and SmallInt values are stored as `Value::Int(i64)` — they use the existing
`(DataType::Int, Value::Int(v)) => buf.extend_from_slice(&v.to_le_bytes())` arm
only if the match is (DataType, Value) pair. If the match is on DataType alone to decide
encoding width, add:
```rust
(DataType::TinyInt, Value::Int(v)) => buf.push(*v as u8),   // 1-byte wire
(DataType::SmallInt, Value::Int(v)) => buf.extend_from_slice(&(*v as i16).to_le_bytes()),
```

> Note: check the actual encoding used for Bool (1 byte) and Int (4 bytes LE) to ensure
> TinyInt sends 1 byte and SmallInt sends 2 bytes as MySQL protocol expects.

### Verification — must compile clean, all existing tests pass
```bash
limactl shell axiomdb -- bash -c "source ~/.cargo/env && CARGO_TARGET_DIR=\$HOME/axiomdb-target cargo nextest run -p axiomdb-sql -p axiomdb-network -p axiomdb-catalog 2>&1 | tail -10"
```

### Commit
```
feat(fase-24): wire TinyInt/SmallInt through DataType↔ColumnType conversions

Step 3 of specs/fase-24/plan-24.1-integer-types.md
```

---

## Step 4 — Fix parser: TINYINT→TinyInt, SMALLINT→SmallInt, add BIGSERIAL

**Goal:** Parser emits correct DataType for TINYINT/SMALLINT; recognizes BIGSERIAL.
**Files:** `crates/axiomdb-sql/src/parser/ddl.rs`

### Implementation outline

In `parse_column_type` (around line 1220):

**TINYINT fix:**
```rust
// Before:
Token::Ident(s) if s.eq_ignore_ascii_case("TINYINT") => {
    p.advance();
    eat_optional_length(p)?;
    (DataType::Bool, 0, false)  // BUG
}

// After:
Token::Ident(s) if s.eq_ignore_ascii_case("TINYINT") => {
    p.advance();
    eat_optional_length(p)?;
    (DataType::TinyInt, 0, false)
}
```

**SMALLINT fix:**
```rust
// Before:
Token::Ident(s) if s.eq_ignore_ascii_case("SMALLINT") => {
    p.advance();
    eat_optional_length(p)?;
    (DataType::Int, 0, false)  // wrong type

// After:
Token::Ident(s) if s.eq_ignore_ascii_case("SMALLINT") => {
    p.advance();
    eat_optional_length(p)?;
    (DataType::SmallInt, 0, false)
}
```

**BIGSERIAL — add after SERIAL:**
```rust
Token::Ident(s) if s.eq_ignore_ascii_case("BIGSERIAL") => {
    p.advance();
    (DataType::BigInt, 0, true)  // auto_increment = true
}
```

(The third element in the tuple is `auto_increment: bool`, matching SERIAL's pattern.)

### Verification
```bash
limactl shell axiomdb -- bash -c "source ~/.cargo/env && CARGO_TARGET_DIR=\$HOME/axiomdb-target cargo nextest run -p axiomdb-sql 2>&1 | tail -10"
```

### Commit
```
feat(fase-24): fix TINYINT→TinyInt, SMALLINT→SmallInt; add BIGSERIAL in parser

Step 4 of specs/fase-24/plan-24.1-integer-types.md
```

---

## Step 5 — Range check on insert + integration tests + wire smoke

**Goal:** Overflow is rejected; all spec done-criteria verified; wire test updated.
**Files:** `crates/axiomdb-sql/src/table.rs`, `crates/axiomdb-sql/tests/integration_integer_types.rs`, `tools/wire-test.py`

### Range check implementation

The coerce function for TinyInt/SmallInt should validate i8/i16 range.
Option A: add range check inside `coerce_values` after coercing to DataType::TinyInt:

In `coerce_values`, after the `coerce(v, target, CoercionMode::Strict)?` call, add a
post-coerce range check:
```rust
(DataType::TinyInt, Value::Int(n)) if !(i8::MIN as i64..=i8::MAX as i64).contains(n) => {
    return Err(DbError::InvalidValue(format!("value {} out of range for TINYINT", n)));
}
(DataType::SmallInt, Value::Int(n)) if !(i16::MIN as i64..=i16::MAX as i64).contains(n) => {
    return Err(DbError::InvalidValue(format!("value {} out of range for SMALLINT", n)));
}
```

Or add this in the `coerce` function in `eval/coerce.rs` when target is TinyInt/SmallInt.
Locate the coerce function first and add it there — single point of enforcement covering
both `coerce_values` and `coerce_values_with_ctx`.

### Integration tests

```rust
// crates/axiomdb-sql/tests/integration_integer_types.rs
#[test]
fn tinyint_insert_and_select() { ... }

#[test]
fn tinyint_overflow_returns_error() {
    // INSERT 128 into TINYINT col → DbError::InvalidValue
}

#[test]
fn tinyint_boundary_values_accepted() {
    // INSERT -128, 127 → OK
}

#[test]
fn tinyint_null_in_nullable_column() { ... }

#[test]
fn smallint_insert_and_select() { ... }

#[test]
fn smallint_overflow_returns_error() {
    // INSERT 40000 → error
}

#[test]
fn smallint_boundary_values_accepted() {
    // INSERT -32768, 32767 → OK
}

#[test]
fn bigserial_auto_increments() {
    // CREATE TABLE t (id BIGSERIAL PRIMARY KEY)
    // INSERT 3 rows without specifying id
    // SELECT id → 1, 2, 3
}

#[test]
fn show_columns_reports_correct_type_names() {
    // SHOW COLUMNS FROM t → Type column = "tinyint" / "smallint"
}
```

### Wire smoke test additions in tools/wire-test.py

```python
# TINYINT wire type
cur.execute("CREATE TABLE IF NOT EXISTS t_tinyint (id INT PRIMARY KEY, val TINYINT)")
cur.execute("INSERT INTO t_tinyint VALUES (1, 42)")
cur.execute("SELECT val FROM t_tinyint WHERE id = 1")
row = cur.fetchone()
assert row[0] == 42, f"expected 42, got {row[0]}"
# Check field type descriptor = 0x01 TINY
desc = cur.description[0]
assert desc[1] == 1, f"expected TINY (1) for TINYINT, got {desc[1]}"  # pymysql type code

# SMALLINT wire type  
cur.execute("CREATE TABLE IF NOT EXISTS t_smallint (id INT PRIMARY KEY, val SMALLINT)")
cur.execute("INSERT INTO t_smallint VALUES (1, 1000)")
cur.execute("SELECT val FROM t_smallint WHERE id = 1")
row = cur.fetchone()
assert row[0] == 1000, f"expected 1000, got {row[0]}"
desc = cur.description[0]
assert desc[1] == 2, f"expected SHORT (2) for SMALLINT, got {desc[1]}"  # pymysql type code
```

### Final workspace verification
```bash
limactl shell axiomdb -- bash -c "source ~/.cargo/env && CARGO_TARGET_DIR=\$HOME/axiomdb-target cargo nextest run --workspace 2>&1 | tail -5"
limactl shell axiomdb -- bash -c "source ~/.cargo/env && CARGO_TARGET_DIR=\$HOME/axiomdb-target cargo clippy --workspace -- -D warnings 2>&1 | tail -10"
limactl shell axiomdb -- bash -c "source ~/.cargo/env && CARGO_TARGET_DIR=\$HOME/axiomdb-target cargo fmt --check 2>&1 | tail -5"
```

### Wire test
```bash
pkill axiomdb-server 2>/dev/null; sleep 1
cargo build -p axiomdb-server --release --manifest-path /Users/cristian/nexusdb/.claude/worktrees/priceless-montalcini-945319/Cargo.toml --target-dir /tmp/axiomdb-wt-target
AXIOMDB_SERVER_BIN=/tmp/axiomdb-wt-target/release/axiomdb-server python3 tools/wire-test.py
```

### Commit
```
feat(fase-24): complete 24.1 — TINYINT/SMALLINT/BIGSERIAL with range checks and wire tests

Implements specs/fase-24/spec-24.1-integer-types.md
Plan: specs/fase-24/plan-24.1-integer-types.md
Tests: 9 integration + 4 wire assertions
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| Wire encoding width mismatch (TINYINT sends 4 bytes instead of 1) | medium | verify in step 3 reading actual value serialization code; fix before committing |
| coerce() doesn't pass DataType through to the range-check point | low | read coerce.rs in step 5 before writing check; insert at the correct layer |
| CAST(x AS TINYINT) path different from INSERT | low | test CAST separately in integration tests |

## Rollback plan

If abandoned mid-way: leave on current branch. Steps 1-2 only add enum variants (additive,
no behavior change). Steps 3-4 fix parser + match arms. Revert with `git revert` of individual commits.

## Estimated effort

Total: ~2 hours
- Step 1: 15 min (types.rs only)
- Step 2: 15 min (schema_table.rs + test update)
- Step 3: 45 min (6 files, many match arms — mechanical but careful)
- Step 4: 15 min (ddl.rs, 3 simple edits)
- Step 5: 45 min (coerce + 9 integration tests + wire smoke + closing protocol)
