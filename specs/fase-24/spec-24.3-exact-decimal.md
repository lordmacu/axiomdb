# Spec: 24.3 — Exact DECIMAL

Phase: 24 — Complete Type System
Task: Exact DECIMAL — precision/scale enforcement, correct division, ROUND/TRUNC
Status: approved

## Context

`Value::Decimal(i128, u8)` is fully wired (codec, coerce, wire, arithmetic) since 24.3b.
Three gaps remain: `DECIMAL(p,s)` column parameters are parsed and discarded; division
is integer-mantissa truncation instead of proper decimal division; ROUND/TRUNC functions
are absent. This subphase closes all three. Depends on no other open subphase.

## Goal

Make `DECIMAL(precision, scale)` behave as a real SQL type: persist `p` and `s` in the
catalog, enforce them at insert/coerce time (round to `s`, reject integer overflow),
produce correct decimal division results, and provide `ROUND(x, n)` / `TRUNC(x, n)`.

## Non-goals

- No change to `Value::Decimal(i128, u8)` storage format — on-disk format unchanged.
- No full SQL-standard arithmetic type-inference rules for mixed precision/scale operations.
- No `DECIMAL` in indexes (already works via i128 comparison; no change needed).
- No `rust_decimal` arithmetic for Add/Sub/Mul — only for rounding and division.
- No `MONEY` type changes — Money has its own scale handling.

## Behavior

### type_len encoding for DECIMAL(p, s)

`ColumnDef.type_len` (u16) encodes precision and scale:

```
type_len = (precision as u16) << 8 | (scale as u16)
```

Special cases:
- Bare `DECIMAL` (no args): `precision = 10`, `scale = 0` → `type_len = 0x0A00`
- `DECIMAL(p)`: `precision = p`, `scale = 0`
- `DECIMAL(p, s)`: `precision = p`, `scale = s`
- Constraints: `1 ≤ p ≤ 38`, `0 ≤ s ≤ p`

The upper byte of `type_len` is `precision`; the lower byte is `scale`. A `type_len`
of `0` (legacy rows from before 24.3) is treated as `precision=10, scale=0`.

### Parser changes

`eat_optional_precision_scale` is replaced by `parse_decimal_params` which returns
`(precision: u8, scale: u8)`. The parser returns `type_len = (p << 8) | s` in the
tuple instead of `0`.

Validation at parse time:
- `p` must be 1–38; `s` must be 0–p.

### Enforcement at insert / coerce

When coercing a `Value` to a `DECIMAL` column that has `type_len != 0`, after the
standard coerce:

1. **Round to target scale `s`**: if the stored scale differs from `s`, round using
   ROUND_HALF_UP. `Value::Decimal(m, s_actual)` → rescale to `s`.
2. **Check integer part fits in `p - s` digits**: the integer part of the value must
   have ≤ `p - s` digits. If it overflows, return `DbError::InvalidValue`.

The coerce path is in the executor's insert/update helpers, not in `coerce_api.rs`
(which remains schema-free). A new function `enforce_decimal_precision` in
`axiomdb-sql/src/table.rs` or a shared executor helper performs this check.

```rust
/// Rounds `Value::Decimal(m, s_src)` to `scale` decimal places (ROUND_HALF_UP),
/// then checks that the integer part fits within `precision - scale` digits.
/// Returns `DbError::InvalidValue` on overflow.
pub fn enforce_decimal_precision(
    v: Value,
    precision: u8,
    scale: u8,
) -> Result<Value, DbError>;
```

### Division fix

Current: `m1 / m2` (integer division of mantissas, scale = s1).

New: scale the numerator by `10^(s2 + 6)` before dividing, then set result scale to
`s1 + 6` (giving up to 6 extra fractional digits). Cap scale at 38.

```
result_mantissa = (m1 × 10^(s2 + extra)) / m2
result_scale    = s1 + extra  (capped at 38)
where extra = min(6, 38 - s1)
```

This matches MySQL's behaviour: `10.00 / 3.00` → `3.333333` (6 extra digits).

If the column has a declared scale `s`, the caller enforces precision/scale after division.

### ROUND and TRUNC functions

Added to `eval/functions/math.rs`:

```sql
ROUND(x, n)   -- rounds x to n decimal places (ROUND_HALF_UP); n defaults to 0
ROUND(x)      -- equivalent to ROUND(x, 0)
TRUNC(x, n)   -- truncates x towards zero to n decimal places; n defaults to 0
TRUNCATE(x,n) -- MySQL alias for TRUNC(x, n)
```

For `Value::Decimal(m, s)` inputs, use `rust_decimal` only for the rounding step:
convert to `rust_decimal::Decimal`, call `.round_dp_with_strategy()`, convert back.

For `Value::Real` and `Value::Float` inputs, use `f64::round()` and `f64::trunc()`.
For `Value::Int` / `Value::BigInt`, return unchanged.

### SHOW COLUMNS / INFORMATION_SCHEMA

`SHOW COLUMNS` displays `decimal(p,s)` instead of `decimal` when `type_len != 0`.

`information_schema.COLUMNS`:
- `DATA_TYPE` = `'decimal'`
- `NUMERIC_PRECISION` = `p`
- `NUMERIC_SCALE` = `s`

### Error cases

| Input | Column | Expected error | Message |
|-------|--------|----------------|---------|
| `'1234.56'` | `DECIMAL(5,2)` | `DbError::InvalidValue` | `"value 1234.56 overflows DECIMAL(5,2): integer part has 4 digits, max 3"` |
| `p=0` in DDL | — | `DbError::ParseError` | `"DECIMAL precision must be between 1 and 38"` |
| `s > p` in DDL | — | `DbError::ParseError` | `"DECIMAL scale cannot exceed precision"` |
| Division by zero | — | `DbError::DivisionByZero` | (existing) |

## Edge cases

- [ ] Bare `DECIMAL` (no params) — treated as `DECIMAL(10, 0)`, accepts integers only; `'1.5'` rounds to `2`
- [ ] `DECIMAL(38, 38)` — entire value is fractional; integer part must be 0
- [ ] `DECIMAL(1, 0)` — single digit; `'9'` OK, `'10'` rejected
- [ ] Insert `NULL` into nullable DECIMAL column — passes through unchanged
- [ ] Negative values — `-123.46` in `DECIMAL(5,2)`: integer part is 3 digits (`123`), within `p-s=3`
- [ ] Scale truncation: `DECIMAL(5,2)` receiving `Value::Decimal(100000, 5)` = `1.00000` → rounds to `1.00`
- [ ] `ROUND(x)` with no second argument — defaults to 0 decimal places
- [ ] `ROUND(NULL, 2)` — returns NULL
- [ ] Division of two DECIMAL values where scale would exceed 38 — cap at 38

## Performance budget

No new hot path. Rounding via `rust_decimal` is called only at insert time (coerce),
not during SELECT. Division is already O(1). No benchmark target needed.

## Dependencies

- `rust_decimal = "1"` added to `axiomdb-types/Cargo.toml`
- Depends on: 24.3b completed (✅ already done)
- Blocks: nothing (standalone subphase)

## Done criteria

- [ ] `DECIMAL(p,s)` DDL stores type_len correctly; `SHOW COLUMNS` displays `decimal(p,s)`
- [ ] Insert value that overflows precision returns `DbError::InvalidValue`
- [ ] Insert value with extra scale is rounded (HALF_UP) to target scale
- [ ] `SELECT 10.00 / 3.00` returns `3.333333` (6 fractional digits)
- [ ] `ROUND(1.2345, 2)` returns `1.23`; `ROUND(1.2350, 2)` returns `1.24` (HALF_UP)
- [ ] `TRUNC(1.999, 1)` returns `1.9`; `TRUNCATE(x, n)` alias works
- [ ] `ROUND(x)` with no `n` defaults to 0
- [ ] All edge cases above have integration tests
- [ ] `cargo nextest run -p axiomdb-types -p axiomdb-sql` — clean
- [ ] `cargo clippy --workspace -- -D warnings` — clean
- [ ] `cargo fmt --check` — clean
- [ ] Wire smoke: 6+ assertions for DECIMAL(p,s), division, ROUND/TRUNC

## References

- `docs/fase-24.md` — Phase 24 overview
- `crates/axiomdb-types/src/coerce_helpers.rs:227` — existing `parse_text_to_decimal`
- `crates/axiomdb-sql/src/eval/ops.rs:755` — existing `decimal_arith`
- MySQL manual: `DECIMAL` type, `ROUND()`, `TRUNCATE()`
- PostgreSQL: `numeric` type, division scale rules
