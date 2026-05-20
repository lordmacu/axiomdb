# Spec: 24.7 — TIMESTAMPTZ

Phase: 24 — Complete Type System
Task: TIMESTAMPTZ — UTC timestamp with timezone marker
Status: approved

## Context

`Value::Timestamp(i64)` stores microseconds since the UTC epoch and is fully wired
(codec, coerce, wire, functions). However there is no distinct type for "timestamp
with timezone" — both inputs and outputs lose timezone context. This subphase adds
`TIMESTAMPTZ` as a first-class type: same 8-byte UTC storage, distinct ColumnType tag,
timezone-aware text parsing, and `+00` display suffix. The design principle from
`db.md` line 115: "always store in µs UTC internally, convert on display."

Depends on: Phase 24.3 ✅. Next: 24.8 INTERVAL.

## Goal

Make `TIMESTAMPTZ` / `TIMESTAMP WITH TIME ZONE` DDL columns, INSERT literals with
timezone offsets, and SELECT display with `+00` suffix all work correctly end-to-end.

## Non-goals

- No IANA timezone name lookup (`AT TIME ZONE 'America/New_York'`) — deferred.
  Only `'UTC'` and numeric offsets (`'+05:30'`, `'-08:00'`) supported.
- No per-session timezone setting (`SET timezone = 'UTC+5'`) — deferred to Phase 17.
- No `TIMETZ` column type — deferred to a future subphase.
- No change to `Value::Timestamp` storage or display — backward compatible.
- No change to `now()` return type — it keeps returning `Value::Timestamp`.
  Users use `now()::TIMESTAMPTZ` or `CAST(NOW() AS TIMESTAMPTZ)`.

## Behavior

### ColumnType discriminant

`ColumnType::TimestampTz = 22` — next available slot after `Float32 = 21`.

Update the comment in `schema_database.rs` from:
> Discriminants 0, 22-254, and 255 are invalid; 1-21 are valid (Float32 = 21)

to:
> Discriminants 0, 23-254, and 255 are invalid; 1-22 are valid (TimestampTz = 22)

### Value variant

```rust
/// SQL TIMESTAMPTZ — microseconds since 1970-01-01 00:00:00 UTC.
/// Always stored as UTC; any timezone offset in the input literal is converted
/// to UTC at parse time and the offset is discarded.
TimestampTz(i64),
```

Identical internal representation to `Value::Timestamp(i64)`. The distinct variant
allows the executor to choose timezone-annotated display.

### Parser changes (`axiomdb-sql/src/parser/ddl.rs`)

In `parse_data_type`, add before the `other` catch-all:

```
Token::TyTimestamp | Token::TyDatetime + peek "WITH" "TIME" "ZONE"
    → DataType::TimestampTz
Token::TyTimestamp | Token::TyDatetime (no "WITH TIME ZONE")
    → DataType::Timestamp   (unchanged)
Ident("TIMESTAMPTZ")
    → DataType::TimestampTz
Ident("TIMESTAMP") + peek WITH TIME ZONE
    → DataType::TimestampTz
```

Parsing rule:
1. If current token is `Token::TyTimestamp` or `Ident("TIMESTAMP")`:
   - peek ahead for keyword sequence `WITH TIME ZONE` (case-insensitive idents)
   - if found: consume the 3 tokens, return `DataType::TimestampTz`
   - otherwise: return `DataType::Timestamp` (unchanged)
2. If current token is `Ident("TIMESTAMPTZ")`: consume, return `DataType::TimestampTz`

No new lexer token required — `TIMESTAMPTZ` handled as an `Ident` (same as SMALLINT etc.).

### Codec (`axiomdb-types/src/codec.rs`)

`Value::TimestampTz(t)` encodes as **8 bytes LE** — identical to `Value::Timestamp(t)`.
Tag in the codec dispatch uses `DataType::TimestampTz` for encode/decode routing.

```
Byte layout (TimestampTz column, 8 bytes):
  offset  size  field    description
  0       8     micros   i64 LE — µs since 1970-01-01 00:00:00 UTC
```

Compatibility rule: same 8-byte format as Timestamp; the distinction is solely in the
`ColumnType` tag stored in the catalog.

### Text parsing — `parse_text_to_timestamptz_micros`

New function in `axiomdb-types/src/coerce_helpers.rs` (no chrono dependency,
pure arithmetic following the Hinnant algorithm already in the file):

```rust
pub(crate) fn parse_text_to_timestamptz_micros(
    s: &str,
    mode: CoercionMode,
) -> Result<i64, DbError>;
```

Accepted formats (all produce UTC µs):

| Input example | Notes |
|---|---|
| `'2024-01-15 12:30:45'` | No offset → treat as UTC |
| `'2024-01-15 12:30:45.123456'` | Fractional seconds (up to 6 digits) |
| `'2024-01-15 12:30:45+00'` | UTC explicit |
| `'2024-01-15 12:30:45+05:30'` | IST → subtract 5h30m to get UTC |
| `'2024-01-15 12:30:45-08:00'` | PST → add 8h to get UTC |
| `'2024-01-15 12:30:45Z'` | Z = UTC |
| `'2024-01-15T12:30:45+05:30'` | ISO 8601 T separator |
| `'2024-01-15 12:30:45+05'` | Offset without minutes |

Steps:
1. Parse `YYYY-MM-DD` prefix using `ymd_to_days_checked` (existing)
2. Parse separator: space or `T`
3. Parse `HH:MM:SS` (validated ranges: H 0–23, M/S 0–59)
4. Parse optional fractional seconds `.ddddddd` (up to 6 digits, pad right with zeros)
5. Parse optional timezone suffix:
   - `Z` or `+00` or `+00:00` → offset_micros = 0
   - `+HH` or `+HH:MM` → offset_micros = +(HH*3600 + MM*60) * 1_000_000
   - `-HH` or `-HH:MM` → offset_micros = -(HH*3600 + MM*60) * 1_000_000
   - absent → offset_micros = 0 (treat as UTC)
6. Compute: `utc_micros = days * 86_400_000_000 + hms_micros - offset_micros`
7. Return `utc_micros`

Validation:
- H must be 0–23, M/S must be 0–59
- Offset hours must be 0–14, offset minutes must be 0–59
- Result must fit in i64 (overflow → `DbError::InvalidValue`)

### Coercion (`axiomdb-types/src/coerce_api.rs`)

Add to the `coerce` match:

```rust
// Text → TimestampTz
(Value::Text(s), DataType::TimestampTz) => {
    let micros = parse_text_to_timestamptz_micros(&s, mode)?;
    Ok(Value::TimestampTz(micros))
}
// Timestamp → TimestampTz (treat naive timestamp as UTC)
(Value::TimestampTz(t), DataType::TimestampTz) => Ok(Value::TimestampTz(t)),
(Value::Timestamp(t), DataType::TimestampTz) => Ok(Value::TimestampTz(t)),
// TimestampTz → Timestamp (strip tz marker, keep UTC µs)
(Value::TimestampTz(t), DataType::Timestamp) => Ok(Value::Timestamp(t)),
// Date → TimestampTz (midnight UTC)
(Value::Date(d), DataType::TimestampTz) => {
    let micros = (d as i64)
        .checked_mul(86_400_000_000_i64)
        .ok_or_else(|| DbError::InvalidCoercion { ... })?;
    Ok(Value::TimestampTz(micros))
}
```

### AT TIME ZONE operator / function stub

Adds `AT TIME ZONE zone_expr` expression support. In the executor's expression
evaluator (or as a built-in binary operator in `eval/ops.rs`):

```rust
// Expr::AtTimeZone { ts_expr, zone_expr }
// OR handled as a function: AT_TIME_ZONE(ts, zone)
```

Semantics:
- `Value::Timestamp(t) AT TIME ZONE 'UTC'` → `Value::TimestampTz(t)` (no conversion)
- `Value::Timestamp(t) AT TIME ZONE '+05:30'` → `Value::TimestampTz(t - offset_micros)` (convert to UTC)
- `Value::TimestampTz(t) AT TIME ZONE 'UTC'` → `Value::TimestampTz(t)` (identity)
- `Value::TimestampTz(t) AT TIME ZONE '+05:30'` → `Value::TimestampTz(t)` (already UTC, identity)
- Any other zone name string → `DbError::NotImplemented { feature: "IANA timezone lookup" }`

Zone string parsing: same offset parser as text-to-timestamptz. `'UTC'` and `'utc'` are aliases for `'+00:00'`.

Parser AST: add `Expr::AtTimeZone { operand: Box<Expr>, zone: Box<Expr> }`. Parser
recognizes `expr AT TIME ZONE expr` as a postfix binary expression (lower precedence
than comparison, higher than AND).

### Display (`axiomdb-network/src/mysql/result.rs`)

Add `format_timestamptz(micros: i64) -> String`:

```
"YYYY-MM-DD HH:MM:SS+00"              — when µs fraction is zero
"YYYY-MM-DD HH:MM:SS.ffffff+00"       — when µs fraction is non-zero
```

Example: `1705315845_123456` → `"2024-01-15 10:30:45.123456+00"`

Reuse the arithmetic from `format_timestamp` (already in result.rs); append `+00`.

Add arm to the `value_to_text` / `format_value` dispatch:
```rust
Value::TimestampTz(t) => format_timestamptz(*t),
```

### SHOW COLUMNS

`SHOW COLUMNS` and `INFORMATION_SCHEMA.COLUMNS`:
- `DATA_TYPE` / `Type` column: `"timestamptz"` (lowercase, no space, PostgreSQL style)

Touch `ddl_show.rs` and `exec_entry.rs` (same two paths as DECIMAL 24.3).

### column_type_to_data_type + column_data_types

In `axiomdb-sql/src/table.rs`, add:
```rust
ColumnType::TimestampTz => DataType::TimestampTz,
```
to both `column_type_to_data_type` and `column_data_types`.

### Comparison operators

In `axiomdb-sql/src/eval/ops.rs`, add `TimestampTz` arms to `<`, `>`, `<=`, `>=`, `=`, `<>`:

```rust
(Value::TimestampTz(a), Value::TimestampTz(b)) => a.cmp(b),
(Value::TimestampTz(a), Value::Timestamp(b))   => a.cmp(b),  // permissive only
(Value::Timestamp(a),   Value::TimestampTz(b)) => a.cmp(b),  // permissive only
```

### Wire binary protocol (`axiomdb-network/src/mysql/prepared.rs`)

`TimestampTz` uses the same MySQL binary encoding as `Timestamp` (field type
`MYSQL_TYPE_DATETIME = 0x0C`, 11-byte packed year/month/day/hour/min/sec/µs).

### Integration tests

New file: `tests/integration_timestamptz.rs`

Tests required:
1. `CREATE TABLE t (ts TIMESTAMPTZ)` — DDL round-trip
2. `SHOW COLUMNS FROM t` → type displayed as `"timestamptz"`
3. INSERT `'2024-01-15 12:00:00+05:30'` → SELECT shows `'2024-01-15 06:30:00+00'`
4. INSERT `'2024-01-15 12:00:00Z'` → SELECT shows `'2024-01-15 12:00:00+00'`
5. INSERT `'2024-01-15 12:00:00'` (no tz) → stored/displayed as UTC
6. INSERT `'2024-01-15 12:00:45.123456+00'` → fractional seconds preserved
7. `CAST('2024-01-01 00:00:00+00' AS TIMESTAMPTZ)` works
8. `CAST(NOW() AS TIMESTAMPTZ)` works
9. `SELECT ts > '2024-01-01 00:00:00+00'::TIMESTAMPTZ` comparison works
10. `SELECT ts AT TIME ZONE 'UTC'` returns same value
11. `SELECT ts AT TIME ZONE '+05:30'` accepted (converted to UTC internally)
12. `SELECT '2024-01-15 12:00:00+05:30'::TIMESTAMP AT TIME ZONE '+05:30'` → TimestampTz
13. `SELECT TIMESTAMP '2024-01-15 12:00:00' AT TIME ZONE 'America/NY'` → NotImplemented error
14. NULL in TIMESTAMPTZ column — passes through as Null

### Error cases

| Input | Expected error | Notes |
|---|---|---|
| `'not-a-timestamp'::TIMESTAMPTZ` | `DbError::InvalidCoercion` | bad format |
| `'2024-13-01 00:00:00'::TIMESTAMPTZ` | `DbError::InvalidCoercion` | month=13 |
| `'2024-01-01 25:00:00'::TIMESTAMPTZ` | `DbError::InvalidCoercion` | hour=25 |
| `'2024-01-01 00:00:00+15:00'::TIMESTAMPTZ` | `DbError::InvalidCoercion` | offset > 14h |
| `ts AT TIME ZONE 'Europe/Madrid'` | `DbError::NotImplemented` | IANA name |

## Edge cases

- [ ] No timezone suffix → treated as UTC (no error, even in strict mode)
- [ ] `+00:00`, `+00`, `Z`, `UTC` → all equivalent, produce same UTC µs
- [ ] Fractional seconds with 1–6 digits (pad right to 6 digits: `'.1'` = 100000 µs)
- [ ] Fractional seconds with 7+ digits → truncate to 6 (not an error)
- [ ] Negative UTC µs (timestamps before 1970-01-01) — e.g., `'1969-12-31 23:59:59+00'`
- [ ] `i64::MAX` / `i64::MIN` overflow in offset arithmetic → `DbError::InvalidValue`
- [ ] CAST from `Value::Timestamp` → `Value::TimestampTz` (implicit: treat naive as UTC)
- [ ] CAST from `Value::TimestampTz` → `Value::Timestamp` (strip marker, keep UTC µs)
- [ ] NULL → Null passthrough at every coerce path

## On-disk format

```
Byte layout (ColumnType::TimestampTz = 22):
  offset  size  field    description
  0       8     micros   i64 LE — µs since 1970-01-01 00:00:00 UTC

Identical to Timestamp (ColumnType = 7). Distinguished only by catalog column type tag.
```

Backward compatibility: existing Timestamp rows are unaffected. A TIMESTAMPTZ column
cannot be confused with a Timestamp column because the catalog ColumnType byte differs.

## Performance budget

No new hot path. Text parse is O(len) and called only at INSERT/coerce time.
Same 8-byte codec as Timestamp — zero additional overhead on scans.

## Dependencies

- Depends on: Phase 24.3 ✅ (pattern established)
- `axiomdb-types`: no new dependencies (pure arithmetic, no chrono)
- `axiomdb-sql`: already has `chrono 0.4` but it is NOT needed here
- Blocks: 24.8 INTERVAL (uses Timestamp/TimestampTz as arithmetic operands)

## Done criteria

- [ ] `ColumnType::TimestampTz = 22` added; `TryFrom<u8>` updated; comment updated
- [ ] `Value::TimestampTz(i64)` variant exists in `value.rs`
- [ ] Codec encodes/decodes `TimestampTz` as 8 bytes LE
- [ ] Parser accepts `TIMESTAMPTZ`, `TIMESTAMP WITH TIME ZONE`
- [ ] `parse_text_to_timestamptz_micros` handles all accepted formats (see table above)
- [ ] Offset conversion correct: `+05:30` subtracts 5h30m from wall time
- [ ] `SHOW COLUMNS` shows type as `"timestamptz"`
- [ ] `format_timestamptz` appends `+00` (and `.ffffff` when non-zero fraction)
- [ ] All 14 integration tests pass
- [ ] Error cases return the correct `DbError` variants
- [ ] All edge cases have a test
- [ ] `cargo nextest run -p axiomdb-types -p axiomdb-catalog -p axiomdb-sql` — clean
- [ ] `cargo clippy --workspace -- -D warnings` — clean
- [ ] `cargo fmt --check` — clean
- [ ] Wire smoke test: ≥8 assertions covering DDL, INSERT with offset, SELECT display, CAST

## References

- `db.md` line 115: "TIMESTAMPTZ: always store in µs UTC internally"
- `research/postgres/src/include/datatype/timestamp.h`: `typedef int64 TimestampTz`
- `crates/axiomdb-types/src/coerce_helpers.rs:374` — `ymd_to_days_checked` (reuse)
- `crates/axiomdb-network/src/mysql/result.rs:727` — `format_timestamp` (extend)
- Spec 24.3 — pattern reference for adding new ColumnType
- SQL standard ISO 9075-2:2016 §4.6.3 — datetime with time zone
