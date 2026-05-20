# Plan: 24.7 — TIMESTAMPTZ

Phase: 24 — Complete Type System
Task: TIMESTAMPTZ — UTC timestamp with timezone marker
Spec: specs/fase-24/spec-24.7-timestamptz.md
Status: in-progress

## Summary

Seven steps, each touching one crate boundary. Order: catalog tag → types codec+coerce
→ SQL parser+ops → AT TIME ZONE lowering → SHOW COLUMNS → wire display → integration
tests. Steps 1-2 are pure Rust (no SQL execution needed); step 3 compiles but needs
no server to test; step 4 is the trickiest (expression parser); steps 5-7 are glue
and validation. TDD throughout: test written first for each step.

## Dependencies

Must be done first:
- [x] spec-24.7-timestamptz.md approved

Blocks:
- [ ] 24.8 INTERVAL (needs TimestampTz in coerce_api for `timestamp + interval` result type)

## Affected files

New:
- `tests/integration_timestamptz.rs` — 14 integration tests

Modified:
- `crates/axiomdb-catalog/src/schema_database.rs` — `ColumnType::TimestampTz = 22`
- `crates/axiomdb-types/src/value.rs` — `Value::TimestampTz(i64)`
- `crates/axiomdb-types/src/codec.rs` — encode/decode TimestampTz
- `crates/axiomdb-types/src/coerce_helpers.rs` — `parse_text_to_timestamptz_micros`
- `crates/axiomdb-types/src/coerce_api.rs` — 5 new coerce arms
- `crates/axiomdb-sql/src/parser/ddl.rs` — TIMESTAMPTZ / TIMESTAMP WITH TIME ZONE
- `crates/axiomdb-sql/src/table.rs` — `column_type_to_data_type` + `column_data_types`
- `crates/axiomdb-sql/src/eval/ops.rs` — comparison operators for TimestampTz
- `crates/axiomdb-sql/src/parser/expr.rs` — AT TIME ZONE postfix → lowered function
- `crates/axiomdb-sql/src/eval/functions/datetime.rs` — `__at_time_zone` internal function
- `crates/axiomdb-sql/src/executor/ddl_show.rs` — SHOW COLUMNS type name
- `crates/axiomdb-sql/src/executor/exec_entry.rs` — SHOW COLUMNS via exec_entry path
- `crates/axiomdb-network/src/mysql/result.rs` — `format_timestamptz`, value display
- `crates/axiomdb-network/src/mysql/prepared.rs` — binary protocol encoding
- `tools/wire-test.py` — ≥8 new wire assertions

---

## Step 1 — ColumnType::TimestampTz = 22 (catalog)

**Goal:** Reserve discriminant 22 for TIMESTAMPTZ in the catalog.
**Files:** `crates/axiomdb-catalog/src/schema_database.rs`, `src/schema.rs` (test)

### Test to add

In `crates/axiomdb-catalog/src/schema.rs` tests, add `ColumnType::TimestampTz` to the
`variants` array in `test_column_type_roundtrip_all_variants`:

```rust
ColumnType::TimestampTz,
```

Also update `test_column_type_invalid_discriminant`:
```rust
// 23 is now invalid (22 = TimestampTz); 22 was previously invalid
assert!(ColumnType::try_from(0).is_err());
assert!(ColumnType::try_from(23).is_err());
assert!(ColumnType::try_from(255).is_err());
```

### Implementation

In `schema_database.rs`:

```rust
Float32 = 21,    // existing
TimestampTz = 22, // SQL TIMESTAMPTZ — i64 µs UTC (Phase 24.7)
```

Add to `TryFrom<u8>`:
```rust
22 => Ok(Self::TimestampTz),
```

Update the comment:
```
// Discriminants 0, 23-254, and 255 are invalid; 1-22 are valid (TimestampTz = 22)
```

### Verification

```bash
./tools/vm.sh test axiomdb-catalog
./tools/vm.sh clippy axiomdb-catalog
```

### Commit

```
feat(fase-24): 24.7 step 1 — ColumnType::TimestampTz = 22
```

---

## Step 2 — Value::TimestampTz + codec + parse helper + coerce (axiomdb-types)

**Goal:** Store, encode, decode, and coerce TIMESTAMPTZ values.
**Files:** `value.rs`, `codec.rs`, `coerce_helpers.rs`, `coerce_api.rs`

### Tests to add (unit, in axiomdb-types)

```rust
// In crates/axiomdb-types/src/coerce_helpers.rs or a test module

// Codec roundtrip
#[test]
fn timestamptz_codec_roundtrip() {
    let v = Value::TimestampTz(1_705_315_845_123_456); // 2024-01-15 10:30:45.123456 UTC
    let row = vec![v.clone()];
    let types = vec![DataType::TimestampTz];
    let encoded = encode_row(&row, &types).unwrap();
    let decoded = decode_row(&encoded, &types).unwrap();
    assert_eq!(decoded[0], v);
}

// parse_text_to_timestamptz_micros
#[test]
fn parse_timestamptz_with_positive_offset() {
    // 12:00:00+05:30 → UTC 06:30:00
    let micros = parse_text_to_timestamptz_micros("2024-01-15 12:00:00+05:30",
        CoercionMode::Permissive).unwrap();
    // 2024-01-15 06:30:00 UTC
    let expected_micros = parse_text_to_timestamptz_micros("2024-01-15 06:30:00+00",
        CoercionMode::Permissive).unwrap();
    assert_eq!(micros, expected_micros);
}

#[test]
fn parse_timestamptz_z_suffix() {
    let a = parse_text_to_timestamptz_micros("2024-01-15 12:00:00Z",
        CoercionMode::Permissive).unwrap();
    let b = parse_text_to_timestamptz_micros("2024-01-15 12:00:00+00:00",
        CoercionMode::Permissive).unwrap();
    assert_eq!(a, b);
}

#[test]
fn parse_timestamptz_no_offset_treated_as_utc() {
    let a = parse_text_to_timestamptz_micros("2024-01-15 12:00:00",
        CoercionMode::Permissive).unwrap();
    let b = parse_text_to_timestamptz_micros("2024-01-15 12:00:00+00",
        CoercionMode::Permissive).unwrap();
    assert_eq!(a, b);
}

#[test]
fn parse_timestamptz_fractional_seconds() {
    let micros = parse_text_to_timestamptz_micros("2024-01-15 12:00:00.123456+00",
        CoercionMode::Permissive).unwrap();
    // Check fractional part: 0.123456 s = 123456 µs
    assert_eq!(micros % 1_000_000, 123_456);
}

#[test]
fn parse_timestamptz_negative_offset() {
    // 12:00:00-08:00 → UTC 20:00:00
    let micros = parse_text_to_timestamptz_micros("2024-01-15 12:00:00-08:00",
        CoercionMode::Permissive).unwrap();
    let expected = parse_text_to_timestamptz_micros("2024-01-15 20:00:00+00",
        CoercionMode::Permissive).unwrap();
    assert_eq!(micros, expected);
}

#[test]
fn parse_timestamptz_invalid_hour() {
    assert!(parse_text_to_timestamptz_micros("2024-01-15 25:00:00+00",
        CoercionMode::Permissive).is_err());
}

#[test]
fn parse_timestamptz_invalid_offset_too_large() {
    assert!(parse_text_to_timestamptz_micros("2024-01-15 12:00:00+15:00",
        CoercionMode::Permissive).is_err());
}
```

### Implementation

**`value.rs`** — add variant after `Timestamp`:
```rust
/// SQL TIMESTAMPTZ — microseconds since 1970-01-01 00:00:00 UTC.
/// Always stored as UTC; any timezone offset in the input is converted
/// to UTC at parse time and the offset is discarded.
TimestampTz(i64),
```

Add to `Display` impl:
```rust
Value::TimestampTz(t) => write!(f, "TimestampTz({})", t),
```

**`codec.rs`** — in encode match:
```rust
(Value::TimestampTz(t), DataType::TimestampTz) => buf.extend_from_slice(&t.to_le_bytes()),
```

In fixed_size match:
```rust
DataType::TimestampTz => 8,
```

In decode match:
```rust
DataType::TimestampTz => {
    let bytes = &encoded[pos..pos + 8];
    let t = i64::from_le_bytes(bytes.try_into().unwrap());
    (Value::TimestampTz(t), 8)
}
```

In value_data_type (infer DataType from Value):
```rust
Value::TimestampTz(_) => DataType::TimestampTz,
```

In column_type inference (for arrays/composites):
```rust
Value::TimestampTz(_) => array_codec::ColumnType::Timestamp, // reuse wire tag
```

**`coerce_helpers.rs`** — new public(crate) function:
```rust
pub(crate) fn parse_text_to_timestamptz_micros(
    s: &str,
    _mode: CoercionMode,
) -> Result<i64, DbError>
```

Algorithm:
1. Trim whitespace
2. Parse `YYYY-MM-DD` via `ymd_to_days_checked` (existing helper)
3. Require space or `T` separator
4. Parse `HH:MM:SS`: bytes[11..13], [14..16], [17..19] after separator
   - Validate H 0–23, M 0–59, S 0–59
5. Parse optional fractional `.ffffff` (up to 6 digits, pad right with zeros)
6. Parse optional timezone:
   - `Z`, `+00`, `+00:00` → offset_micros = 0
   - `+HH` or `+HH:MM` → offset_micros = +(H*3600 + M*60) * 1_000_000; validate H≤14, M≤59
   - `-HH` or `-HH:MM` → negate; same validation
   - absent → offset_micros = 0
7. Compute: `days * 86_400_000_000 + h*3_600_000_000 + m*60_000_000 + s*1_000_000 + frac_micros - offset_micros`
8. Return checked result (overflow → `DbError::InvalidValue`)

**`coerce_api.rs`** — add 5 arms:
```rust
// Text → TimestampTz
(Value::Text(s), DataType::TimestampTz) => {
    let micros = parse_text_to_timestamptz_micros(&s, mode)?;
    Ok(Value::TimestampTz(micros))
}
// Identity
(Value::TimestampTz(t), DataType::TimestampTz) => Ok(Value::TimestampTz(t)),
// Timestamp (naive) → TimestampTz: treat as UTC
(Value::Timestamp(t), DataType::TimestampTz) => Ok(Value::TimestampTz(t)),
// TimestampTz → Timestamp: strip marker
(Value::TimestampTz(t), DataType::Timestamp) => Ok(Value::Timestamp(t)),
// Date → TimestampTz: midnight UTC
(Value::Date(d), DataType::TimestampTz) => {
    let micros = (d as i64).checked_mul(86_400_000_000_i64)
        .ok_or_else(|| DbError::InvalidCoercion {
            from: "Date".into(), to: "TIMESTAMPTZ".into(),
            value: d.to_string(),
            reason: "days × 86400000000 overflows i64".into(),
        })?;
    Ok(Value::TimestampTz(micros))
}
```

### Verification

```bash
./tools/vm.sh test axiomdb-types
./tools/vm.sh clippy axiomdb-types
```

### Commit

```
feat(fase-24): 24.7 step 2 — Value::TimestampTz + codec + parse + coerce
```

---

## Step 3 — Parser: TIMESTAMPTZ / TIMESTAMP WITH TIME ZONE + table.rs + comparison ops

**Goal:** SQL DDL recognizes the new type; runtime can compare TimestampTz values.
**Files:** `parser/ddl.rs`, `table.rs`, `eval/ops.rs`

### Tests to add

In `crates/axiomdb-sql/tests/integration_ddl_parser.rs`, add:
```rust
#[test]
fn parse_timestamptz_type() {
    let sql = "CREATE TABLE t (ts TIMESTAMPTZ, ts2 TIMESTAMP WITH TIME ZONE)";
    let stmt = parse_one(sql).unwrap();
    // assert both columns have DataType::TimestampTz
}
```

In `eval/ops.rs` unit tests (or inline):
```rust
// TimestampTz comparison
assert!(Value::TimestampTz(100) < Value::TimestampTz(200)); // via eval
```

### Implementation

**`parser/ddl.rs`** — in `parse_data_type`, replace the existing `TyTimestamp` arm:

```rust
Token::TyTimestamp | Token::TyDatetime => {
    p.advance();
    // TIMESTAMP WITH TIME ZONE → TimestampTz
    if matches!(p.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("WITH")) {
        let saved = p.pos; // save position for rollback
        p.advance(); // consume WITH
        if matches!(p.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("TIME")) {
            p.advance(); // consume TIME
            if matches!(p.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("ZONE")) {
                p.advance(); // consume ZONE
                return Ok(ParsedDataType::simple(DataType::TimestampTz));
            }
        }
        p.pos = saved; // rollback: "WITH" belongs to something else
    }
    (DataType::Timestamp, 0, false)
}
Token::Ident(s) if s.eq_ignore_ascii_case("TIMESTAMPTZ") => {
    p.advance();
    (DataType::TimestampTz, 0, false)
}
Token::Ident(s) if s.eq_ignore_ascii_case("TIMESTAMP") => {
    p.advance();
    if matches!(p.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("WITH")) {
        // same WITH TIME ZONE lookahead as above
        // ... (same 3-token sequence)
        return Ok(ParsedDataType::simple(DataType::TimestampTz));
    }
    (DataType::Timestamp, 0, false)
}
```

Note: `ParsedDataType::simple(dt)` is a shorthand for `ParsedDataType { data_type: dt, type_len: 0, is_char: false, ndims: 0, size_hints: vec![] }`. If no such shorthand exists, construct inline.

**`table.rs`** — add to both `column_type_to_data_type` and `column_data_types`:
```rust
ColumnType::TimestampTz => DataType::TimestampTz,
```

Also add a `data_type_to_column_type` mapping if that function exists:
```rust
DataType::TimestampTz => ColumnType::TimestampTz,
```

**`eval/ops.rs`** — add `TimestampTz` arms to all comparison operators (`<`, `>`, `<=`, `>=`, `=`, `<>`). Follow the exact same pattern used for `Timestamp`:

```rust
// Homogeneous comparison
(Value::TimestampTz(a), Value::TimestampTz(b)) => ...
// Mixed (permissive): treat Timestamp as UTC TimestampTz
(Value::TimestampTz(a), Value::Timestamp(b))   => a.cmp(b)  // permissive only
(Value::Timestamp(a),   Value::TimestampTz(b)) => a.cmp(b)  // permissive only
```

Also add `Value::TimestampTz` to any `ORDER BY` sort key handling if it's not caught by the comparison arms.

### Verification

```bash
./tools/vm.sh test axiomdb-sql -- --test integration_ddl_parser
./tools/vm.sh clippy axiomdb-sql
```

### Commit

```
feat(fase-24): 24.7 step 3 — parser TIMESTAMPTZ + table.rs + comparison ops
```

---

## Step 4 — AT TIME ZONE: parser lowering + eval function

**Goal:** `expr AT TIME ZONE 'zone'` works as a postfix expression.
**Files:** `parser/expr.rs`, `eval/functions/datetime.rs`

### Strategy

Lower `AT TIME ZONE` at parse time into a regular internal function call:
```
ts AT TIME ZONE zone_expr  →  Expr::Function { name: "__at_time_zone", args: [ts, zone_expr] }
```

This avoids adding a new `Expr` variant (which propagates changes across eval, analyzer,
planner). The double-underscore prefix marks it as an internal-only function (never
suggested by autocomplete, rejected in user-defined function names).

### Tests to add

In the integration DDL parser tests (or a new `integration_at_time_zone.rs`):
```rust
#[test]
fn at_time_zone_utc() {
    let sql = "SELECT ts AT TIME ZONE 'UTC' FROM t";
    let stmt = parse_one(sql).unwrap();
    // assert the projection contains Expr::Function { name: "__at_time_zone", ... }
}
```

Runtime test in integration_timestamptz.rs (step 7).

### Implementation

**`parser/expr.rs`** — in `parse_is_null`, after the `IS` block (before `Ok(expr)`):

```rust
// AT TIME ZONE postfix
if matches!(p.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("AT")) {
    if matches!(p.peek_at(1), Token::Ident(s) if s.eq_ignore_ascii_case("TIME")) {
        if matches!(p.peek_at(2), Token::Ident(s) if s.eq_ignore_ascii_case("ZONE")) {
            p.advance(); // AT
            p.advance(); // TIME
            p.advance(); // ZONE
            let zone_expr = parse_predicate(p)?; // parse the zone string
            return Ok(Expr::Function {
                name: "__at_time_zone".into(),
                args: vec![expr, zone_expr],
                distinct: false,
                // any other fields that Function has
            });
        }
    }
}
Ok(expr)
```

Check the `Expr::Function` variant fields by reading `src/expr.rs` before coding.

**`eval/functions/datetime.rs`** — add arm:

```rust
"__at_time_zone" if args.len() == 2 => {
    let ts_val = crate::eval::eval(&args[0], row)?;
    let zone_val = crate::eval::eval(&args[1], row)?;
    let zone_str = match &zone_val {
        Value::Text(s) => s.as_str(),
        _ => return Err(DbError::InvalidValue {
            reason: "AT TIME ZONE zone must be a string literal".into(),
        }),
    };
    eval_at_time_zone(ts_val, zone_str)
}
```

New private function `eval_at_time_zone(ts: Value, zone: &str) -> Result<Value, DbError>`:

```rust
fn eval_at_time_zone(ts: Value, zone: &str) -> Result<Value, DbError> {
    let offset_micros = parse_zone_offset_micros(zone)?; // same logic as in coerce_helpers
    let utc_micros = match ts {
        Value::Timestamp(t) => t - offset_micros,
        Value::TimestampTz(t) => t, // already UTC, ignore offset
        Value::Null => return Ok(Value::Null),
        _ => return Err(DbError::InvalidValue {
            reason: format!("AT TIME ZONE requires a timestamp, got {ts:?}"),
        }),
    };
    Ok(Value::TimestampTz(utc_micros))
}

fn parse_zone_offset_micros(zone: &str) -> Result<i64, DbError> {
    let z = zone.trim();
    if z.eq_ignore_ascii_case("UTC") || z == "+00" || z == "+00:00" || z == "Z" {
        return Ok(0);
    }
    let (sign, rest) = if let Some(r) = z.strip_prefix('+') {
        (1i64, r)
    } else if let Some(r) = z.strip_prefix('-') {
        (-1i64, r)
    } else {
        return Err(DbError::NotImplemented {
            feature: format!("IANA timezone lookup for '{z}'; use UTC or a numeric offset like '+05:30'"),
        });
    };
    // Parse HH or HH:MM
    let (h, m) = if let Some((hh, mm)) = rest.split_once(':') {
        (hh.parse::<i64>().map_err(|_| err())?, mm.parse::<i64>().map_err(|_| err())?)
    } else {
        (rest.parse::<i64>().map_err(|_| err())?, 0)
    };
    if h > 14 || m > 59 {
        return Err(DbError::InvalidValue { reason: "timezone offset out of range".into() });
    }
    Ok(sign * (h * 3_600_000_000 + m * 60_000_000))
}
```

### Verification

```bash
./tools/vm.sh test axiomdb-sql
./tools/vm.sh clippy axiomdb-sql
```

### Commit

```
feat(fase-24): 24.7 step 4 — AT TIME ZONE postfix operator (lowered to __at_time_zone)
```

---

## Step 5 — SHOW COLUMNS display (executor)

**Goal:** `SHOW COLUMNS FROM t` shows `timestamptz` for TimestampTz columns.
**Files:** `executor/ddl_show.rs`, `executor/exec_entry.rs`

### Implementation

Search both files for where `ColumnType::Float32` was added in 24.2 and add the parallel arm:

```rust
ColumnType::TimestampTz => "timestamptz".to_string(),
```

Also update `column_sql_type_display` (or equivalent) function used for
`INFORMATION_SCHEMA.COLUMNS.DATA_TYPE`.

### Verification

```bash
./tools/vm.sh test axiomdb-sql -- integration_ddl
./tools/vm.sh clippy axiomdb-sql
```

### Commit

```
feat(fase-24): 24.7 step 5 — SHOW COLUMNS displays "timestamptz"
```

---

## Step 6 — Wire: format_timestamptz + binary protocol

**Goal:** Text and binary MySQL wire output correctly format TIMESTAMPTZ values.
**Files:** `crates/axiomdb-network/src/mysql/result.rs`, `prepared.rs`

### Implementation

**`result.rs`** — add `format_timestamptz(micros: i64) -> String`:

```rust
pub(crate) fn format_timestamptz(micros: i64) -> String {
    let frac = micros.rem_euclid(1_000_000);
    let secs = micros / 1_000_000;
    let (year, month, day) = days_to_ymd(secs / 86400);
    let rem = secs.rem_euclid(86400);
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;
    if frac == 0 {
        format!("{year:04}-{month:02}-{day:02} {h:02}:{m:02}:{s:02}+00")
    } else {
        format!("{year:04}-{month:02}-{day:02} {h:02}:{m:02}:{s:02}.{frac:06}+00")
    }
}
```

Add to the value-to-text dispatch (wherever `Value::Timestamp(t)` is handled):
```rust
Value::TimestampTz(t) => format_timestamptz(*t),
```

**`prepared.rs`** — `TimestampTz` uses the same binary encoding as `Timestamp`
(MySQL `MYSQL_TYPE_DATETIME = 0x0C`, 11-byte packed). Add:
```rust
Value::TimestampTz(t) => encode_binary_datetime(*t, buf),
```
where `encode_binary_datetime` is the existing function used for `Value::Timestamp`.

### Verification

```bash
./tools/vm.sh test axiomdb-network
./tools/vm.sh clippy axiomdb-network
```

### Commit

```
feat(fase-24): 24.7 step 6 — wire format_timestamptz + binary protocol
```

---

## Step 7 — Integration tests + wire smoke

**Goal:** All 14 spec tests pass; ≥8 wire assertions pass.
**Files:** `tests/integration_timestamptz.rs`, `tools/wire-test.py`

### Integration tests (Lima VM, `cargo nextest run`)

```rust
// tests/integration_timestamptz.rs

// Test 1: DDL round-trip
fn test_create_table_timestamptz() { ... }

// Test 2: SHOW COLUMNS type name
fn test_show_columns_type() { ... } // asserts "timestamptz"

// Test 3: INSERT with positive offset, SELECT shows UTC
fn test_insert_positive_offset() {
    // INSERT '2024-01-15 12:00:00+05:30' → SELECT '2024-01-15 06:30:00+00'
}

// Test 4: INSERT with Z suffix
fn test_insert_z_suffix() {
    // INSERT '2024-01-15 12:00:00Z' → SELECT '2024-01-15 12:00:00+00'
}

// Test 5: INSERT with no tz, stored as UTC
fn test_insert_no_tz() { ... }

// Test 6: INSERT with fractional seconds
fn test_insert_fractional_seconds() {
    // INSERT '2024-01-15 12:00:45.123456+00' → frac preserved
}

// Test 7: CAST Text → TIMESTAMPTZ
fn test_cast_text() { ... }

// Test 8: CAST NOW() → TIMESTAMPTZ
fn test_cast_now() { ... }

// Test 9: Comparison operator
fn test_comparison() {
    // SELECT ts > '2024-01-01 00:00:00+00'::TIMESTAMPTZ
}

// Test 10: AT TIME ZONE 'UTC' identity
fn test_at_time_zone_utc() { ... }

// Test 11: AT TIME ZONE with positive offset
fn test_at_time_zone_positive() { ... }

// Test 12: TIMESTAMP + AT TIME ZONE
fn test_timestamp_at_time_zone() { ... }

// Test 13: AT TIME ZONE with IANA name → error
fn test_at_time_zone_iana_error() {
    // assert result is DbError::NotImplemented
}

// Test 14: NULL passthrough
fn test_null_timestamptz() { ... }
```

### Wire smoke additions (`tools/wire-test.py`)

Prepend new section BEFORE existing tests (keep all existing tests intact):

```python
# ── 24.7 TIMESTAMPTZ ────────────────────────────────────────────────────────
cur.execute("""
    CREATE TABLE IF NOT EXISTS test_timestamptz (
        id INT AUTO_INCREMENT PRIMARY KEY,
        ts TIMESTAMPTZ
    )
""")
cur.execute("INSERT INTO test_timestamptz (ts) VALUES ('2024-01-15 12:00:00+05:30')")
cur.execute("SELECT ts FROM test_timestamptz")
row = cur.fetchone()
assert row[0] == "2024-01-15 06:30:00+00", f"expected UTC conversion, got {row[0]!r}"

cur.execute("DELETE FROM test_timestamptz")
cur.execute("INSERT INTO test_timestamptz (ts) VALUES ('2024-01-15 12:00:00Z')")
cur.execute("SELECT ts FROM test_timestamptz")
row = cur.fetchone()
assert row[0] == "2024-01-15 12:00:00+00", f"expected +00 suffix, got {row[0]!r}"

cur.execute("SELECT ts AT TIME ZONE 'UTC' FROM test_timestamptz")
row = cur.fetchone()
assert row[0] is not None, "AT TIME ZONE UTC returned NULL"

# SHOW COLUMNS type check
cur.execute("SHOW COLUMNS FROM test_timestamptz")
cols = {r[0]: r[1] for r in cur.fetchall()}
assert cols["ts"] == "timestamptz", f"expected timestamptz, got {cols['ts']!r}"

# Comparison
cur.execute("DELETE FROM test_timestamptz")
cur.execute("INSERT INTO test_timestamptz (ts) VALUES ('2024-06-01 00:00:00+00')")
cur.execute("SELECT COUNT(*) FROM test_timestamptz WHERE ts > '2024-01-01 00:00:00+00'")
row = cur.fetchone()
assert row[0] == 1, f"comparison failed, got {row[0]}"

# Negative offset
cur.execute("DELETE FROM test_timestamptz")
cur.execute("INSERT INTO test_timestamptz (ts) VALUES ('2024-01-15 12:00:00-08:00')")
cur.execute("SELECT ts FROM test_timestamptz")
row = cur.fetchone()
assert row[0] == "2024-01-15 20:00:00+00", f"negative offset, got {row[0]!r}"

# Fractional seconds
cur.execute("DELETE FROM test_timestamptz")
cur.execute("INSERT INTO test_timestamptz (ts) VALUES ('2024-01-15 12:00:45.123456+00')")
cur.execute("SELECT ts FROM test_timestamptz")
row = cur.fetchone()
assert ".123456" in row[0], f"fractional seconds lost, got {row[0]!r}"

# NULL
cur.execute("DELETE FROM test_timestamptz")
cur.execute("INSERT INTO test_timestamptz (ts) VALUES (NULL)")
cur.execute("SELECT ts FROM test_timestamptz")
row = cur.fetchone()
assert row[0] is None, f"expected NULL, got {row[0]!r}"

print("✅ 24.7 TIMESTAMPTZ: all 8 wire assertions pass")
```

### Final verification against spec done criteria

- [ ] `ColumnType::TimestampTz = 22` added; TryFrom updated; comment updated
- [ ] `Value::TimestampTz(i64)` in value.rs
- [ ] Codec encodes/decodes as 8 bytes LE
- [ ] Parser: TIMESTAMPTZ, TIMESTAMP WITH TIME ZONE
- [ ] parse_text_to_timestamptz_micros: all formats in spec table
- [ ] Offset conversion correct (+05:30 → -5h30m to wall time)
- [ ] SHOW COLUMNS → "timestamptz"
- [ ] format_timestamptz appends +00 (and .ffffff when non-zero fraction)
- [ ] 14 integration tests pass
- [ ] Error cases return correct DbError variants
- [ ] All edge cases tested
- [ ] `cargo nextest run -p axiomdb-types -p axiomdb-catalog -p axiomdb-sql` clean
- [ ] `cargo clippy --workspace -- -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] Wire smoke: 8 assertions pass

### Commit

```
feat(fase-24): 24.7 step 7 — integration tests + wire smoke (24.7 complete)
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|---|---|---|
| Parser: `TIMESTAMP WITH TIME ZONE` mid-sentence rollback needed | low | save/restore `p.pos` before consuming WITH |
| Offset overflow for dates near i64 boundaries | low | checked_mul in arithmetic; test with negative epoch |
| AT TIME ZONE `parse_zone_offset_micros` shares logic with `coerce_helpers` — drift | medium | extract to shared fn in `coerce_helpers`, call from both sites |
| Wire binary DATETIME encoding for TimestampTz | low | reuse existing `encode_binary_datetime` used for Timestamp |

## Rollback plan

1. `git reset --hard <commit before step 1>` (all changes are in 7 incremental commits)
2. Or cherry-pick only the steps that are clean

## Estimated effort

Total: ~5-6 hours
- Step 1: 20 min
- Step 2: 90 min (parse_text_to_timestamptz_micros is the bulk)
- Step 3: 45 min
- Step 4: 60 min (AT TIME ZONE parse + eval)
- Step 5: 20 min
- Step 6: 30 min
- Step 7: 60 min (14 tests + wire script)
