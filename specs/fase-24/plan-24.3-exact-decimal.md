# Plan: 24.3 — Exact DECIMAL

Phase: 24 — Complete Type System
Task: Exact DECIMAL — precision/scale enforcement, correct division, ROUND/TRUNC
Spec: specs/fase-24/spec-24.3-exact-decimal.md
Status: in-progress

## Summary

Three independent improvements to the existing `Value::Decimal(i128, u8)` type:
(1) capture `DECIMAL(p,s)` parameters in the parser and encode them in `type_len`,
enforce precision/scale at insert using `rust_decimal` for rounding;
(2) fix division to produce 6 extra fractional digits instead of truncating;
(3) add `Value::Decimal` arms to the existing `ROUND`/`TRUNC` functions.
Steps are ordered: parser first (no dep), enforcement second (needs `rust_decimal`),
division+functions third (also needs `rust_decimal`), close last.

## Dependencies

Must be done first:
- [x] 24.3b completed — `Value::Decimal(i128, u8)` fully wired

Blocks:
- nothing

## Affected files

New files:
- `crates/axiomdb-sql/tests/integration_decimal_precision.rs` — 12+ integration tests

Modified files:
- `crates/axiomdb-sql/Cargo.toml` — add `rust_decimal = "1"`
- `crates/axiomdb-sql/src/parser/ddl.rs` — `parse_decimal_params`, type_len encoding
- `crates/axiomdb-sql/src/executor/ddl_show.rs` — display `decimal(p,s)`
- `crates/axiomdb-sql/src/executor/information_schema_exec.rs` — NUMERIC_PRECISION/SCALE
- `crates/axiomdb-sql/src/table.rs` — `enforce_decimal_precision` + call in coerce_values*
- `crates/axiomdb-sql/src/eval/ops.rs` — fix `decimal_arith` Div arm
- `crates/axiomdb-sql/src/eval/functions/numeric.rs` — Decimal arms in `round` + `truncate`
- `tools/wire-test.py` — 6+ new [24.3 decimal] assertions
- `docs/fase-24.md` — Phase 24.3 section
- `docs/progreso.md` — mark 24.3 ✅

---

## Step 1 — Parser captures p,s → type_len; SHOW COLUMNS displays decimal(p,s)

**Goal:** `DECIMAL(10, 2)` DDL stores `type_len = 0x0A02`; `SHOW COLUMNS` shows `decimal(10,2)`.
**Files:** `parser/ddl.rs`, `executor/ddl_show.rs`, `executor/information_schema_exec.rs`

### Test to add

```rust
// crates/axiomdb-sql/tests/integration_decimal_precision.rs
#[test]
fn decimal_column_params_stored_and_shown() {
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE t (price DECIMAL(10,2), qty DECIMAL(5,0), bare DECIMAL)",
        &mut storage, &mut txn,
    );
    let cols = common::rows(common::run("SHOW COLUMNS FROM t", &mut storage, &mut txn));
    // Field, Type, Null, Key, Default, Extra
    let types: Vec<String> = cols.iter().map(|r| r[1].to_display()).collect();
    assert_eq!(types[0], "decimal(10,2)");
    assert_eq!(types[1], "decimal(5,0)");
    assert_eq!(types[2], "decimal(10,0)"); // bare DECIMAL defaults to (10,0)
}

#[test]
fn decimal_params_invalid_precision_rejected() {
    let (mut storage, mut txn) = common::setup();
    let r = common::try_run("CREATE TABLE t (x DECIMAL(0,0))", &mut storage, &mut txn);
    assert!(r.is_err());
    let r2 = common::try_run("CREATE TABLE t (x DECIMAL(39,0))", &mut storage, &mut txn);
    assert!(r2.is_err());
    let r3 = common::try_run("CREATE TABLE t (x DECIMAL(5,6))", &mut storage, &mut txn);
    assert!(r3.is_err()); // scale > precision
}
```

### Implementation outline

In `parser/ddl.rs`, replace `eat_optional_precision_scale` call with `parse_decimal_params`:

```rust
Token::TyDecimal | Token::TyNumeric => {
    p.advance();
    let (prec, scale) = parse_decimal_params(p)?;
    let type_len = (prec as u16) << 8 | (scale as u16);
    (DataType::Decimal, type_len, false)
}

/// Returns (precision, scale). Defaults: precision=10, scale=0.
/// Validates: 1≤p≤38, 0≤s≤p.
fn parse_decimal_params(p: &mut Parser) -> Result<(u8, u8), DbError> {
    if !p.eat(&Token::LParen) {
        return Ok((10, 0)); // bare DECIMAL → DECIMAL(10,0)
    }
    let prec = parse_u8_param(p, "precision", 1, 38)?;
    let scale = if p.eat(&Token::Comma) {
        parse_u8_param(p, "scale", 0, prec)?
    } else {
        0
    };
    p.expect(&Token::RParen)?;
    Ok((prec, scale))
}
```

In `ddl_show.rs`, `column_type_to_sql_name` and `scalar_type_to_sql_name`:

```rust
ColumnType::Decimal => {
    if col.type_len != 0 {
        let p = (col.type_len >> 8) as u8;
        let s = (col.type_len & 0xFF) as u8;
        format!("decimal({p},{s})")
    } else {
        "decimal".to_string()
    }
}
```

In `information_schema_exec.rs`, for `NUMERIC_PRECISION` and `NUMERIC_SCALE`:

```rust
ColumnType::Decimal => {
    let p = if col.type_len != 0 { (col.type_len >> 8) as i64 } else { 10 };
    let s = if col.type_len != 0 { (col.type_len & 0xFF) as i64 } else { 0 };
    // NUMERIC_PRECISION column: Value::BigInt(p)
    // NUMERIC_SCALE column: Value::BigInt(s)
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql --test integration_decimal_precision
./tools/vm.sh clippy -p axiomdb-sql -- -D warnings
```

### Commit

```
feat(fase-24): 24.3 step 1 — parser captures DECIMAL(p,s) into type_len
```

---

## Step 2 — Enforcement at insert: round to scale, reject integer overflow

**Goal:** Inserting `'123.456'` into `DECIMAL(5,2)` stores `123.46`; inserting `'1234.56'` rejects.
**Files:** `axiomdb-sql/Cargo.toml`, `table.rs`

### Dependency

Add to `crates/axiomdb-sql/Cargo.toml`:

```toml
rust_decimal = { version = "1", default-features = false, features = ["maths"] }
```

### Test to add

```rust
#[test]
fn decimal_insert_rounds_to_scale() {
    let (mut storage, mut txn) = common::setup();
    common::run("CREATE TABLE t (x DECIMAL(10,2))", &mut storage, &mut txn);
    common::run("INSERT INTO t VALUES ('123.456')", &mut storage, &mut txn);
    let out = common::rows(common::run("SELECT x FROM t", &mut storage, &mut txn));
    assert_eq!(out, vec![vec![Value::Decimal(12346, 2)]]); // ROUND_HALF_UP: .456 → .46
}

#[test]
fn decimal_insert_rounds_half_up() {
    let (mut storage, mut txn) = common::setup();
    common::run("CREATE TABLE t (x DECIMAL(10,2))", &mut storage, &mut txn);
    common::run("INSERT INTO t VALUES ('1.235')", &mut storage, &mut txn);
    let out = common::rows(common::run("SELECT x FROM t", &mut storage, &mut txn));
    assert_eq!(out, vec![vec![Value::Decimal(124, 2)]]); // 1.235 → 1.24
}

#[test]
fn decimal_insert_overflow_precision_rejected() {
    let (mut storage, mut txn) = common::setup();
    common::run("CREATE TABLE t (x DECIMAL(5,2))", &mut storage, &mut txn);
    // 1234.56 has 4 integer digits but max is 5-2=3 → rejected
    let r = common::try_run("INSERT INTO t VALUES ('1234.56')", &mut storage, &mut txn);
    assert!(r.is_err());
}

#[test]
fn decimal_insert_null_passes() {
    let (mut storage, mut txn) = common::setup();
    common::run("CREATE TABLE t (x DECIMAL(5,2))", &mut storage, &mut txn);
    common::run("INSERT INTO t VALUES (NULL)", &mut storage, &mut txn);
    let out = common::rows(common::run("SELECT x FROM t", &mut storage, &mut txn));
    assert_eq!(out, vec![vec![Value::Null]]);
}

#[test]
fn decimal_insert_bare_rounds_to_zero_scale() {
    let (mut storage, mut txn) = common::setup();
    common::run("CREATE TABLE t (x DECIMAL)", &mut storage, &mut txn); // DECIMAL(10,0)
    common::run("INSERT INTO t VALUES ('1.5')", &mut storage, &mut txn);
    let out = common::rows(common::run("SELECT x FROM t", &mut storage, &mut txn));
    assert_eq!(out, vec![vec![Value::Decimal(2, 0)]]); // 1.5 → 2 (ROUND_HALF_UP)
}
```

### Implementation outline

In `axiomdb-sql/src/table.rs`:

```rust
use rust_decimal::Decimal as RustDecimal;
use rust_decimal::RoundingStrategy;

/// Rounds `Value::Decimal(m, s_src)` to `scale` decimal places (ROUND_HALF_UP),
/// then checks that the integer part has ≤ `precision - scale` digits.
pub(crate) fn enforce_decimal_precision(
    v: Value,
    precision: u8,
    scale: u8,
) -> Result<Value, DbError> {
    let Value::Decimal(m, s_src) = v else { return Ok(v) };

    // Convert to rust_decimal for rounding.
    let rd = RustDecimal::from_i128_with_scale(m, s_src as u32);
    let rounded = rd.round_dp_with_strategy(
        scale as u32,
        RoundingStrategy::MidpointAwayFromZero,
    );

    // Convert back to (i128, u8).
    let new_scale = scale;
    let new_mantissa = (rounded * RustDecimal::from(10i128.pow(new_scale as u32)))
        .to_i128()
        .ok_or(DbError::Overflow)?;

    // Check integer part: mantissa / 10^scale must have ≤ (precision - scale) digits.
    let int_part = new_mantissa.unsigned_abs() / 10u128.pow(new_scale as u32);
    let max_int_digits = precision.saturating_sub(new_scale);
    let max_int = 10u128.pow(max_int_digits as u32);
    if int_part >= max_int {
        return Err(DbError::InvalidValue {
            reason: format!(
                "value overflows DECIMAL({precision},{scale}): \
                 integer part has {} digits, max {}",
                int_part.to_string().len(),
                max_int_digits
            ),
        });
    }

    Ok(Value::Decimal(new_mantissa, new_scale))
}
```

In `coerce_values` and `coerce_values_with_ctx`, after the standard coerce for `ColumnType::Decimal`:

```rust
ColumnType::Decimal => {
    let coerced = coerce(v, DataType::Decimal, mode)?;
    if col.type_len != 0 {
        let precision = (col.type_len >> 8) as u8;
        let scale = (col.type_len & 0xFF) as u8;
        enforce_decimal_precision(coerced, precision, scale)?
    } else {
        // Legacy / bare DECIMAL: treat as DECIMAL(10,0)
        enforce_decimal_precision(coerced, 10, 0)?
    }
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql --test integration_decimal_precision
```

### Commit

```
feat(fase-24): 24.3 step 2 — DECIMAL(p,s) enforcement at insert with rust_decimal rounding
```

---

## Step 3 — Division precision fix + ROUND/TRUNC for Decimal

**Goal:** `10.00 / 3.00` → `3.333333`; `ROUND(1.235, 2)` → `1.24`; `TRUNC(1.999, 1)` → `1.9`.
**Files:** `eval/ops.rs`, `eval/functions/numeric.rs`

### Test to add

```rust
#[test]
fn decimal_division_produces_fractional_digits() {
    let (mut storage, mut txn) = common::setup();
    let out = common::rows(common::run(
        "SELECT 10.00 / 3.00",
        &mut storage, &mut txn,
    ));
    // 10.00 (m=1000, s=2) / 3.00 (m=300, s=2) → scale up numerator → 3.333333
    match &out[0][0] {
        Value::Decimal(m, s) => {
            let f = *m as f64 / 10f64.powi(*s as i32);
            assert!((f - 3.333333).abs() < 1e-5, "got {m}×10^-{s} = {f}");
        }
        other => panic!("expected Decimal, got {other:?}"),
    }
}

#[test]
fn decimal_round_half_up() {
    let (mut storage, mut txn) = common::setup();
    let out = common::rows(common::run(
        "SELECT ROUND(1.2345, 2), ROUND(1.2350, 2), ROUND(-1.235, 2)",
        &mut storage, &mut txn,
    ));
    assert_eq!(out[0][0], Value::Decimal(123, 2));   // 1.23
    assert_eq!(out[0][1], Value::Decimal(124, 2));   // 1.24 (HALF_UP)
    assert_eq!(out[0][2], Value::Decimal(-124, 2));  // -1.24
}

#[test]
fn decimal_trunc() {
    let (mut storage, mut txn) = common::setup();
    let out = common::rows(common::run(
        "SELECT TRUNC(1.999, 1), TRUNCATE(1.999, 2), TRUNC(-1.999, 1)",
        &mut storage, &mut txn,
    ));
    assert_eq!(out[0][0], Value::Decimal(19, 1));    // 1.9
    assert_eq!(out[0][1], Value::Decimal(199, 2));   // 1.99
    assert_eq!(out[0][2], Value::Decimal(-19, 1));   // -1.9
}

#[test]
fn decimal_round_no_scale_arg() {
    let (mut storage, mut txn) = common::setup();
    let out = common::rows(common::run(
        "SELECT ROUND(1.6), ROUND(1.4)",
        &mut storage, &mut txn,
    ));
    assert_eq!(out[0][0], Value::Decimal(2, 0));
    assert_eq!(out[0][1], Value::Decimal(1, 0));
}
```

### Implementation outline

**`eval/ops.rs` — `decimal_arith` Div arm:**

```rust
BinaryOp::Div => {
    if m2 == 0 {
        return Err(DbError::DivisionByZero);
    }
    // Scale numerator up to produce extra fractional digits.
    // extra = min(6, 38 - s1) to avoid scale overflow.
    let extra = 6u8.min(38u8.saturating_sub(s1));
    let factor = 10i128.pow((s2 + extra) as u32);
    let scaled_m1 = m1.checked_mul(factor).ok_or(DbError::Overflow)?;
    let result = scaled_m1.checked_div(m2).ok_or(DbError::Overflow)?;
    let result_scale = s1.saturating_add(extra);
    Ok(Value::Decimal(result, result_scale))
}
```

**`eval/functions/numeric.rs` — add `Value::Decimal` arms:**

In the `"round"` arm, after `Value::Real`:
```rust
Value::Decimal(m, s) => {
    let rd = RustDecimal::from_i128_with_scale(m, s as u32);
    let rounded = rd.round_dp_with_strategy(
        decimals,
        RoundingStrategy::MidpointAwayFromZero,
    );
    let new_scale = decimals as u8;
    let new_m = (rounded * RustDecimal::from(10i128.pow(decimals)))
        .to_i128()
        .ok_or(DbError::Overflow)?;
    Ok(Value::Decimal(new_m, new_scale))
}
```

In the `"truncate" | "trunc"` arm, after `Value::Real`:
```rust
Value::Decimal(m, s) => {
    let target_scale = d.max(0) as u8;
    if s <= target_scale {
        // Already at or below target scale — pad with zeros if needed.
        let factor = 10i128.pow((target_scale - s) as u32);
        Ok(Value::Decimal(m.checked_mul(factor).ok_or(DbError::Overflow)?, target_scale))
    } else {
        // Truncate (towards zero): discard extra fractional digits.
        let drop = s - target_scale;
        let divisor = 10i128.pow(drop as u32);
        Ok(Value::Decimal(m / divisor, target_scale))
    }
}
```

Note: both functions use `use rust_decimal::{Decimal as RustDecimal, RoundingStrategy};`
at the top of `numeric.rs`.

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql --test integration_decimal_precision
./tools/vm.sh clippy -p axiomdb-sql -- -D warnings
```

### Commit

```
feat(fase-24): 24.3 step 3 — decimal division precision + ROUND/TRUNC for Decimal values
```

---

## Step 4 — Wire smoke + closing protocol

**Goal:** verify wire-visible behaviour, write docs, close subphase.
**Files:** `tools/wire-test.py`, `docs/fase-24.md`, `docs/progreso.md`, memory files

### Wire assertions to add (`[24.3 decimal]` block)

1. `CREATE TABLE dp (x DECIMAL(8,3))` → ok
2. `INSERT INTO dp VALUES (123.456789)` → ok (rounds to `123.457`)
3. `SELECT x FROM dp` → `123.457`
4. `SELECT 10.00 / 3.00` → starts with `3.333`
5. `SELECT ROUND(1.2345, 2)` → `1.23`
6. `SELECT TRUNC(1.999, 1)` → `1.9`

### Verification against spec done criteria

- [ ] `DECIMAL(p,s)` DDL stores type_len; SHOW COLUMNS shows `decimal(p,s)`
- [ ] Insert overflow → `DbError::InvalidValue`
- [ ] Insert extra scale → rounded HALF_UP
- [ ] `SELECT 10.00/3.00` → `3.333333` (6 fractional digits)
- [ ] `ROUND(1.235, 2)` → `1.24`; `ROUND(1.235, 2)` HALF_UP verified
- [ ] `TRUNC(1.999, 1)` → `1.9`; `TRUNCATE` alias works
- [ ] `ROUND(x)` with no n defaults to 0
- [ ] NULL passes through
- [ ] All edge cases from spec have tests
- [ ] `cargo nextest run --workspace` → clean
- [ ] `cargo clippy --workspace -- -D warnings` → clean
- [ ] `cargo fmt --check` → clean
- [ ] Wire smoke: 6+ assertions pass

### Final commit

```
feat(fase-24): complete 24.3 exact DECIMAL — precision/scale enforcement, correct division, ROUND/TRUNC

Implements specs/fase-24/spec-24.3-exact-decimal.md
- DECIMAL(p,s) stores precision+scale in type_len; enforced at insert via rust_decimal rounding
- Division scales numerator by 10^(s2+6) for 6 extra fractional digits (MySQL-compatible)
- ROUND/TRUNC/TRUNCATE extended to Value::Decimal with ROUND_HALF_UP semantics
- 12 new integration tests; 6 new wire assertions
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| `rust_decimal` i128 conversion edge cases | low | test with 10^38 boundary values |
| Scale cap overflow in division | low | `saturating_add` + cap at 38 |
| Legacy rows with `type_len=0` treated wrong | medium | treat 0 as DECIMAL(10,0) explicitly |
| `coerce_values_with_ctx` warns instead of errors | low | check mode handling after adding enforce call |

## Rollback plan

1. `git reset --hard <commit before step 1>`
2. Spec stays approved; mark status back to `draft` with note

## Estimated effort

Total: ~3 hours
- Step 1 (parser + display): 45 min
- Step 2 (enforcement + rust_decimal): 60 min
- Step 3 (division + functions): 45 min
- Step 4 (wire + close): 30 min
