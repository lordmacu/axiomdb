# Plan: REAL/FLOAT4 separate from DOUBLE (f32 vs f64)

Phase: 24 — Complete type system
Task: 24.2 — Distinct REAL (f32) and DOUBLE (f64) types
Spec: specs/fase-24/spec-24.2-float-real-double.md
Status: in-progress

## Summary

Two-step plan. Step 1 adds `DataType::Float` and `ColumnType::Float32 = 21` to the
type/catalog crates, wiring codec (4-byte f32 LE), coerce (f64→f32 truncation),
and the local ColumnType copy in `array_codec.rs`. Step 2 wires the SQL layer
(parser, executors, wire protocol) and adds integration tests + wire smoke.
`DataType::Real` (f64, `ColumnType::Float = 4`) is untouched — all existing tables
continue to work.

## Dependencies

Must be done first:
- [x] spec-24.2 approved
- [x] plan-24.1 completed (ColumnType::Float32 next tag is 21, local copy pattern known)

Blocks:
- nothing

## Affected files

New files:
- `crates/axiomdb-sql/tests/integration_float_types.rs` — 8 integration tests

Modified files:
- `crates/axiomdb-types/src/types.rs` — `DataType::Float` variant
- `crates/axiomdb-types/src/codec.rs` — decode/validate_type/skip/col_type_to_data_type
- `crates/axiomdb-types/src/coerce_api.rs` — f64→f32 truncation coerce arm
- `crates/axiomdb-types/src/field_patch.rs` — `fixed_encoded_size`: Float32 → 4
- `crates/axiomdb-types/src/array_codec.rs` — local ColumnType::Float32=21 + all arms
- `crates/axiomdb-types/src/array_io.rs` — text-parse arm
- `crates/axiomdb-catalog/src/schema_database.rs` — `ColumnType::Float32 = 21`
- `crates/axiomdb-catalog/src/schema.rs` — roundtrip test + invalid discriminant (21→22)
- `crates/axiomdb-catalog/src/schema_aggregate.rs` — data_type_tag/from_tag
- `crates/axiomdb-sql/src/parser/ddl.rs` — TyReal/TyFloat/FLOAT4 → DataType::Float; FDW arm
- `crates/axiomdb-sql/src/executor/ddl_show.rs` — type name: Float → "real", Real → "double"
- `crates/axiomdb-sql/src/executor/shared.rs` — datatype↔column_type conversions
- `crates/axiomdb-sql/src/executor/information_schema_exec.rs` — IS type names
- `crates/axiomdb-sql/src/executor/fdw_http.rs` — JSON→Float arm
- `crates/axiomdb-sql/src/table.rs` — column_type_to_data_type / coerce arms
- `crates/axiomdb-sql/src/table_ctx.rs` — ZoneMap numeric check
- `crates/axiomdb-sql/src/json_table.rs` — datatype_to_column_type
- `crates/axiomdb-sql/src/values_clause.rs` — datatype_to_column_type
- `crates/axiomdb-sql/src/planner_select.rs` — literal coerce
- `crates/axiomdb-network/src/mysql/result.rs` — wire 0x04 + 4-byte binary encode
- `tools/wire-test.py` — 4 new assertions

---

## Step 1 — Type system: DataType::Float + ColumnType::Float32 + codec

**Goal:** Add Float as a distinct type in axiomdb-types and axiomdb-catalog with correct
4-byte on-disk encoding and f64→f32 coercion.
**Files:** `types.rs`, `codec.rs`, `coerce_api.rs`, `field_patch.rs`, `array_codec.rs`,
`array_io.rs`, `schema_database.rs`, `schema.rs`, `schema_aggregate.rs`

### DataType addition (`types.rs`)

```rust
/// SQL REAL / FLOAT4 — f32 precision, stored as 4-byte LE IEEE 754.
/// Runtime value: Value::Real(f64) (widened on decode). Wire: 0x04 FLOAT.
Float,
```

Add to `name()`: `Self::Float => "REAL".into()`

### ColumnType addition (`schema_database.rs`)

```rust
Float32 = 21,  // SQL REAL/FLOAT4 — 4-byte f32 LE; wire 0x04 FLOAT (Phase 24.2)
// TryFrom: 21 => Ok(Self::Float32)
```

### Local ColumnType in `array_codec.rs`

Add `Float32 = 21` to the local enum, TryFrom, `data_type_to_column_type`:
```rust
crate::types::DataType::Float => ColumnType::Float32,
```

Add to encode_element and decode_element — Float32 uses 4-byte f32:
```rust
ColumnType::Float32 => {
    let bytes: [u8; 4] = buf[..4].try_into().unwrap();
    values.push(Value::Real(f32::from_le_bytes(bytes) as f64));
    4
}
```
Encode:
```rust
ColumnType::Float32 => {
    buf.extend_from_slice(&(*v as f32).to_le_bytes());
}
```

### Codec (`codec.rs`)

`validate_type`: add `(Value::Real(_), DataType::Float)` arm.

`decode_row` (both variants): DataType::Float → read 4 bytes as f32, widen to f64:
```rust
DataType::Float => {
    let bytes: [u8; 4] = row[offset..offset + 4].try_into().unwrap();
    Value::Real(f32::from_le_bytes(bytes) as f64)
}
```
`skip`: `DataType::Float => 4`

`col_type_to_data_type`:
```rust
array_codec::ColumnType::Float32 => DataType::Float,
```

### field_patch (`field_patch.rs`)

```rust
DataType::Float => Some(4),
```

### coerce_api (`coerce_api.rs`)

```rust
(Value::Real(v), DataType::Float) => Ok(Value::Real(*v as f32 as f64)),
(Value::Int(v), DataType::Float) => Ok(Value::Real(*v as f32 as f64)),
(Value::BigInt(v), DataType::Float) => Ok(Value::Real(*v as f32 as f64)),
(Value::Text(s), DataType::Float) => {
    // parse then truncate to f32
    let f: f64 = s.parse().map_err(|_| DbError::InvalidValue {
        reason: format!("cannot parse '{}' as REAL", s),
    })?;
    Ok(Value::Real(f as f32 as f64))
}
```

### array_io (`array_io.rs`)

Add `ColumnType::Float32` to the text-parse arm (same path as Float/f64).

### schema.rs

Update roundtrip test to include Float32, update invalid discriminant test (21 → 22).

### schema_aggregate.rs

`data_type_tag`: Float → 21 (or next free tag)
`data_type_from_tag`: 21 → Float

### Verification

```bash
limactl shell axiomdb -- cargo build -p axiomdb-types -p axiomdb-catalog 2>&1 | grep error
```

### Commit

```
feat(fase-24): 24.2 step 1 — DataType::Float + ColumnType::Float32=21 + codec
```

---

## Step 2 — SQL layer + parser + wire + tests + close

**Goal:** Wire Float through all SQL executor paths, fix parser, wire protocol, add
integration tests and wire smoke, then close the subphase.
**Files:** all axiomdb-sql files + axiomdb-network/result.rs + tests + wire-test.py

### Parser (`ddl.rs`)

Change line ~1192:
```rust
Token::TyReal | Token::TyFloat => {
    p.advance();
    (DataType::Float, 0, false)  // was DataType::Real
}
Token::TyDouble => {
    p.advance();
    (DataType::Real, 0, false)   // unchanged
}
```

Also add `FLOAT4` and `FLOAT8` ident handling if not already present:
```rust
Token::Ident(s) if s.eq_ignore_ascii_case("FLOAT4") => {
    p.advance();
    (DataType::Float, 0, false)
}
Token::Ident(s) if s.eq_ignore_ascii_case("FLOAT8") => {
    p.advance();
    (DataType::Real, 0, false)
}
```

FDW arm `fdw_datatype_to_column_type`: add `DataType::Float => ColumnType::Float32`.

### ddl_show.rs

```rust
ColumnType::Float32 => "real",
// ColumnType::Float (existing) => "double"  ← rename the existing arm label
```

### shared.rs

```rust
DataType::Float => ColumnType::Float32,
// DataType::Real => ColumnType::Float  ← unchanged
ColumnType::Float32 => DataType::Float,
```

### information_schema_exec.rs

```rust
ColumnType::Float32 => "real",
// ColumnType::Float => "double"  ← update existing arm
```

### fdw_http.rs

```rust
ColumnType::Float32 | ColumnType::Float => { /* same path */ }
```

### table.rs (4 arms)

```rust
ColumnType::Float32 => DataType::Float,
```

### table_ctx.rs

```rust
ColumnType::Float32 | ColumnType::Float | ColumnType::BigInt | ...
```

### json_table.rs, values_clause.rs, planner_select.rs

Add `DataType::Float => ColumnType::Float32` arms.

### result.rs (wire)

```rust
DataType::Float => 0x04,   // FLOAT (4 bytes)
// DataType::Real => 0x05  ← unchanged

// column_display_len:
DataType::Float => 12,  // "-3.40282e38"

// encode_binary_cell:
(DataType::Float, Value::Real(v)) => buf.extend_from_slice(&(*v as f32).to_le_bytes()),
```

### Integration tests (`integration_float_types.rs`)

```rust
mod common;
use axiomdb_types::Value;

#[test]
fn real_insert_and_select() { /* REAL column stores/retrieves f32-precision value */ }

#[test]
fn float4_keyword_accepted() { /* FLOAT4 keyword → same as REAL */ }

#[test]
fn float_keyword_accepted() { /* FLOAT keyword → same as REAL */ }

#[test]
fn double_insert_and_select() { /* DOUBLE unchanged — regression */ }

#[test]
fn real_null_in_nullable_column() { /* NULL in REAL column */ }

#[test]
fn real_precision_truncation() {
    /* Insert precise f64 into REAL, retrieve f32-rounded value */
    // 3.1415927 → 3.1415927 as f32 as f64
}

#[test]
fn show_columns_reports_real_and_double() {
    /* SHOW COLUMNS FROM t shows "real" for REAL, "double" for DOUBLE */
}

#[test]
fn double_precision_keyword() { /* DOUBLE PRECISION keyword → DataType::Real */ }
```

### Wire smoke (`tools/wire-test.py`)

```python
# ── 24.2 REAL / DOUBLE ────────────────────────────────────────────────────────
cur.execute("DROP TABLE IF EXISTS _wire_float")
cur.execute("CREATE TABLE _wire_float (r REAL, d DOUBLE)")
cur.execute("INSERT INTO _wire_float VALUES (3.14, 3.141592653589793)")
cur.execute("SELECT r, d FROM _wire_float")
_frow = cur.fetchone()
ok("[24.2 float] REAL column returns f32-range value", abs(_frow[0] - 3.14) < 0.001, _frow[0])
ok("[24.2 float] DOUBLE column returns full precision", abs(_frow[1] - 3.141592653589793) < 1e-10, _frow[1])
cur.execute("SELECT r, d FROM _wire_float")  # metadata check
_fd = cur.description
ok("[24.2 float] REAL wire type is float-family", _fd[0][1] is not None, _fd[0][1])
ok("[24.2 float] DOUBLE wire type is float-family", _fd[1][1] is not None, _fd[1][1])
cur.execute("DROP TABLE IF EXISTS _wire_float")
```

### Verification against spec

- [ ] REAL column → 4-byte on disk, wire 0x04
- [ ] DOUBLE column unchanged → 8-byte on disk, wire 0x05
- [ ] SHOW COLUMNS shows `real` / `double`
- [ ] f32 precision truncation test passes
- [ ] NULL in REAL column works
- [ ] 8 integration tests pass
- [ ] `cargo nextest run --workspace` clean
- [ ] `cargo clippy --workspace -- -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] 4 wire assertions pass

### Commit

```
feat(fase-24): complete subphase 24.2 — REAL/FLOAT4 (f32) distinct from DOUBLE (f64)
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| `ColumnType::Float` name confusion (= f64 despite "Float") | low | comment clearly in schema_database.rs |
| Missed match arm causing compile error | medium | `cargo build` after step 1 catches all |
| `DOUBLE PRECISION` two-token parse | low | check existing parser handles two tokens |

## Rollback plan

`git reset --hard` to plan commit. No catalog migration needed (Float32 is new tag, additive only).

## Estimated effort

Total: 45 min
Step 1: 20 min (types/catalog layer)
Step 2: 25 min (SQL/network + tests)
