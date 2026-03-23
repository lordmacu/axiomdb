# Plan: 4.18b — Type Coercion Matrix

## Files to create/modify

| File | Action | What it does |
|---|---|---|
| `crates/nexusdb-core/src/error.rs` | modify | Add `InvalidCoercion` variant + SQLSTATE 22018 |
| `crates/nexusdb-types/src/coerce.rs` | **create** | `CoercionMode`, `coerce()`, `coerce_for_op()`, all helpers |
| `crates/nexusdb-types/src/lib.rs` | modify | `pub mod coerce` + re-exports |
| `crates/nexusdb-sql/src/eval.rs` | modify | Replace `coerce_numeric`/`coerce_for_compare` with `coerce_for_op` |

No new crate, no new dependency. All changes within the existing workspace.

---

## Algorithm / Data structures

### Step 1 — `DbError::InvalidCoercion`

```rust
// nexusdb-core/src/error.rs — add variant:
#[error("cannot coerce {value} ({from}) to {to}: {reason}")]
InvalidCoercion {
    from:   String,   // e.g. "Text"
    to:     String,   // e.g. "INT"
    value:  String,   // Display of the input value, e.g. "'42abc'"
    reason: String,   // human-readable cause
}

// In sqlstate():
DbError::InvalidCoercion { .. } => "22018",
```

### Step 2 — `CoercionMode` enum

```rust
// nexusdb-types/src/coerce.rs
pub enum CoercionMode {
    Strict,      // default — '42abc'→INT = error
    Permissive,  // MySQL compat — '42abc'→INT = 42
}
```

### Step 3 — `coerce()` — dispatch table

```
coerce(value, target, mode):
  if value == Null                    → Ok(Null)              // NULL passes through
  if value.datatype() == target       → Ok(value)             // identity
  match (value, target):
    // Numeric widening (lossless)
    (Int(n),    BigInt)   → Ok(BigInt(n as i64))
    (Int(n),    Real)     → Ok(Real(n as f64))
    (Int(n),    Decimal)  → Ok(Decimal(n as i128, 0))
    (BigInt(n), Real)     → Ok(Real(n as f64))
    (BigInt(n), Decimal)  → Ok(Decimal(n as i128, 0))
    // Numeric narrowing (may fail)
    (BigInt(n), Int) →
        if n < i32::MIN as i64 || n > i32::MAX as i64:
            Err(InvalidCoercion { reason: "value overflows INT range" })
        else:
            Ok(Int(n as i32))
    // Text → numeric
    (Text(s), Int)     → parse_text_to_bigint(&s, mode).and_then(|n| bigint_to_int(n))
    (Text(s), BigInt)  → parse_text_to_bigint(&s, mode).map(BigInt)
    (Text(s), Real)    → parse_text_to_float(&s, mode).map(Real)
    (Text(s), Decimal) → parse_text_to_decimal(&s, mode).map(|(m,s)| Decimal(m, s))
    // Temporal
    (Date(d), Timestamp) →
        d.checked_mul(86_400_000_000_i64).map(Timestamp)
        .ok_or(InvalidCoercion { reason: "date × 86400000000 overflows i64" })
    // Bool → numeric (permissive only)
    (Bool(b), Int)    if Permissive → Ok(Int(b as i32))
    (Bool(b), BigInt) if Permissive → Ok(BigInt(b as i64))
    (Bool(b), Real)   if Permissive → Ok(Real(if b { 1.0 } else { 0.0 }))
    // Everything else
    _ → Err(InvalidCoercion { from: value.variant_name(), to: target.name(),
                               value: value.to_string(), reason: "no implicit conversion" })
```

### Step 4 — Text parsing helpers

#### `parse_text_to_bigint(s, mode) -> Result<i64, DbError>`

```
trimmed = s.trim()
if trimmed.is_empty() → error (empty string)

Strict:
  trimmed.parse::<i64>()
  on Err → InvalidCoercion { reason: "invalid integer literal '{trimmed}'" }

Permissive (MySQL behavior):
  i = 0
  if trimmed[i] == '-' or '+': record sign, i += 1
  while i < len and trimmed[i] is ascii digit: accumulate digits, i += 1
  if no digits consumed (after optional sign): return Ok(0)  // MySQL: "abc"→0
  parse accumulated digits as u64 (to detect overflow before sign)
  apply sign → i64, check bounds
  if overflow: InvalidCoercion (MySQL also errors on overflow)
  return Ok(value)
```

#### `parse_text_to_float(s, mode) -> Result<f64, DbError>`

```
trimmed = s.trim()
Strict:
  trimmed.parse::<f64>()
  on Err → InvalidCoercion
  if result.is_nan() → InvalidCoercion { reason: "NaN not allowed" }
  return Ok(result)

Permissive:
  find longest prefix that is a valid float (no stdlib function for this;
  scan forward tracking sign, digits, '.', 'e'/'E' ± exponent)
  parse that prefix; if empty → return Ok(0.0)
  reject NaN even in permissive mode
```

#### `parse_text_to_decimal(s, mode) -> Result<(i128, u8), DbError>`

```
trimmed = s.trim()
// format: [-][digits][.][digits]
parse sign, integer_part (digits before '.'), fraction_part (digits after '.')
scale = fraction_part.len()
mantissa = integer_part * 10^scale + fraction_part (as i128)
apply sign
if scale > 38 → InvalidCoercion { reason: "scale exceeds maximum (38)" }
if mantissa overflow → InvalidCoercion
Strict: full string consumed; trailing characters → error
Permissive: stop at first non-numeric character after fraction
```

### Step 5 — `coerce_for_op()` — widening lattice

```
coerce_for_op(l, r):
  match (l, r):
    (Int,     Int)     | (BigInt,  BigInt) |
    (Real,    Real)    | (Decimal, Decimal) → Ok((l, r))     // same type, no-op

    (Int(a),   BigInt(_)) → Ok((BigInt(a as i64), r))
    (BigInt(_), Int(b))   → Ok((l, BigInt(b as i64)))

    (Int(a),   Real(_))   → Ok((Real(a as f64), r))
    (Real(_),  Int(b))    → Ok((l, Real(b as f64)))

    (BigInt(a), Real(_))  → Ok((Real(a as f64), r))
    (Real(_),  BigInt(b)) → Ok((l, Real(b as f64)))

    (Int(a),    Decimal(_, s)) → Ok((Decimal(a as i128, s), r))  // scale from Decimal side
    (Decimal(_, s), Int(b))    → Ok((l, Decimal(b as i128, s)))

    (BigInt(a),    Decimal(_, s)) → Ok((Decimal(a as i128, s), r))
    (Decimal(_, s), BigInt(b))    → Ok((l, Decimal(b as i128, s)))

    _ → Err(InvalidCoercion { from: l.variant_name(), to: r.variant_name(),
                               value: l.to_string(), reason: "no implicit numeric promotion" })
```

Note: `Int→Decimal` uses the **Decimal side's scale** so that `INT(5) + DECIMAL(314, 2)`
produces `DECIMAL(500 + 314, 2) = DECIMAL(814, 2) = 8.14`. This matches step 4 of
the existing `decimal_arith` logic.

### Step 6 — Integrate into `eval.rs`

Replace the two private functions:
```rust
// REMOVE:
fn coerce_numeric(l: Value, r: Value) -> Result<(Value, Value), DbError> { ... }
fn coerce_for_compare(l: Value, r: Value) -> Result<(Value, Value), DbError> {
    coerce_numeric(l, r)
}
```

With:
```rust
// ADD at top of eval.rs:
use nexusdb_types::coerce::coerce_for_op;

// In eval_arithmetic and compare_values, replace calls to coerce_numeric/coerce_for_compare:
let (l, r) = coerce_for_op(l, r)?;
```

The behavior for all previously-handled cases (Int/BigInt/Real/Decimal widening) is
identical. The only behavioral change: previously-unhandled pairs that silently fell
through now return `InvalidCoercion` instead of an unrelated `TypeMismatch`.

---

## Implementation phases

1. **Add `DbError::InvalidCoercion`** to `nexusdb-core/src/error.rs` + map SQLSTATE.
   Verify `cargo check -p nexusdb-core`.

2. **Create `coerce.rs`** with `CoercionMode` enum and skeleton `coerce()` + `coerce_for_op()`
   that return `NotImplemented`. Verify `cargo check -p nexusdb-types`.

3. **Implement `coerce_for_op()`** — numeric widening lattice. Write unit tests.
   Verify all 10+ test cases pass.

4. **Implement `parse_text_to_bigint()`** with both strict and permissive paths.
   Write unit tests covering: empty string, whitespace-only, negative, `"42abc"`,
   `"abc"`, overflow.

5. **Implement `parse_text_to_float()`**. Write unit tests covering: `"3.14"`,
   `"NaN"`, `"-1.5e2"`, empty, garbage.

6. **Implement `parse_text_to_decimal()`**. Write unit tests covering: `"3.14"`,
   `"100"`, `"-0.5"`, scale > 38.

7. **Implement `coerce()` dispatch** — wire up all arms using the helpers above.
   Write unit tests for each cell in the matrix (identity, widening, narrowing,
   text→numeric, date→timestamp, bool→numeric permissive, invalid pairs).

8. **Export from `nexusdb-types/src/lib.rs`**:
   ```rust
   pub mod coerce;
   pub use coerce::{coerce, coerce_for_op, CoercionMode};
   ```

9. **Integrate into `eval.rs`** — remove `coerce_numeric` and `coerce_for_compare`,
   add `use nexusdb_types::coerce::coerce_for_op`, update call sites.
   Verify all existing `eval.rs` tests still pass.

10. **Run full workspace check** — `cargo test --workspace`, `cargo clippy`, `cargo fmt`.

---

## Tests to write

### In `nexusdb-types/src/coerce.rs` (unit tests, no I/O)

```
// Identity
test_coerce_identity_all_types        — each type coerces to itself unchanged
test_coerce_null_to_any_target        — Null → any DataType = Null

// Numeric widening
test_coerce_int_to_bigint             — Int(42) → BigInt(42)
test_coerce_int_to_real               — Int(5) → Real(5.0)
test_coerce_int_to_decimal            — Int(7) → Decimal(7, 0)
test_coerce_bigint_to_real            — BigInt(i64::MAX) → Real(...)
test_coerce_bigint_to_decimal         — BigInt(100) → Decimal(100, 0)

// Narrowing
test_coerce_bigint_to_int_ok          — BigInt(42) → Int(42)
test_coerce_bigint_to_int_min         — BigInt(i32::MIN as i64) → Int(i32::MIN)
test_coerce_bigint_to_int_max         — BigInt(i32::MAX as i64) → Int(i32::MAX)
test_coerce_bigint_to_int_overflow_hi — BigInt(i32::MAX as i64 + 1) → InvalidCoercion
test_coerce_bigint_to_int_overflow_lo — BigInt(i32::MIN as i64 - 1) → InvalidCoercion

// Text → Int
test_coerce_text_int_clean            — "42" → Int(42)
test_coerce_text_int_whitespace       — "  42  " → Int(42)
test_coerce_text_int_negative         — "-7" → Int(-7)
test_coerce_text_int_strict_garbage   — "42abc" → InvalidCoercion (strict)
test_coerce_text_int_permissive_trail — "42abc" → Int(42) (permissive)
test_coerce_text_int_permissive_all   — "abc" → Int(0) (permissive)
test_coerce_text_int_overflow_int     — "99999999999" → INT: InvalidCoercion (overflow)
test_coerce_text_bigint_large         — "99999999999" → BIGINT: BigInt(99999999999)

// Text → Real
test_coerce_text_real_ok              — "3.14" → Real(3.14)
test_coerce_text_real_negative        — "-1.5e2" → Real(-150.0)
test_coerce_text_real_nan             — "NaN" → InvalidCoercion (both modes)
test_coerce_text_real_inf             — "inf" → InvalidCoercion (both modes)
test_coerce_text_real_garbage_strict  — "3.14xyz" → InvalidCoercion (strict)

// Text → Decimal
test_coerce_text_decimal_fraction     — "3.14" → Decimal(314, 2)
test_coerce_text_decimal_integer      — "100" → Decimal(100, 0)
test_coerce_text_decimal_negative     — "-0.5" → Decimal(-5, 1)
test_coerce_text_decimal_scale_max    — 39-digit fraction → InvalidCoercion

// Date → Timestamp
test_coerce_date_epoch                — Date(0) → Timestamp(0)
test_coerce_date_one_day              — Date(1) → Timestamp(86_400_000_000)
test_coerce_date_negative             — Date(-1) → Timestamp(-86_400_000_000)

// Bool permissive
test_coerce_bool_int_permissive       — Bool(true) → Int(1), Bool(false) → Int(0)
test_coerce_bool_int_strict           — Bool(true) → InvalidCoercion (strict)

// Invalid combinations
test_coerce_text_to_date_is_error     — Text → Date: InvalidCoercion (phase 4.19)
test_coerce_real_to_int_is_error      — Real → Int: InvalidCoercion (use CAST)
test_coerce_int_to_text_is_error      — Int → Text: InvalidCoercion
```

### In `nexusdb-types/src/coerce.rs` (unit tests for `coerce_for_op`)

```
test_op_same_types_all               — all same-type pairs return unchanged
test_op_int_bigint                   — (Int(3), BigInt(10)) → (BigInt(3), BigInt(10))
test_op_bigint_int_symmetric         — (BigInt(10), Int(3)) → (BigInt(10), BigInt(3))
test_op_int_real                     — (Int(5), Real(2.0)) → (Real(5.0), Real(2.0))
test_op_bigint_real                  — BigInt → Real widening
test_op_int_decimal                  — Int uses Decimal's scale
test_op_bigint_decimal               — BigInt uses Decimal's scale
test_op_real_decimal_error           — (Real, Decimal) → InvalidCoercion
test_op_text_int_error               — (Text, Int) → InvalidCoercion
test_op_null_error                   — (Null, Int) → InvalidCoercion (Null handled before coerce_for_op)
```

### In `nexusdb-sql/src/eval.rs` (integration — existing tests must still pass)

```
test_eval_int_plus_bigint             — eval(Int(1) + BigInt(999)) → BigInt(1000)
test_eval_int_eq_real                 — eval(Int(5) = Real(5.0)) → Bool(true)
test_eval_int_lt_real                 — eval(Int(3) < Real(3.5)) → Bool(true)
test_eval_int_plus_decimal            — eval(Int(2) + Decimal(314, 2)) → Decimal(514, 2)
test_eval_incompatible_types_error    — eval(Int(1) + Text("a")) → Err(InvalidCoercion)
```

---

## Anti-patterns to avoid

- **DO NOT** add `CoercionMode` to `coerce_for_op` — operator widening is always
  mode-independent. Text→numeric coercion only happens at column assignment time
  (executor, Phase 4.5), not in binary expressions.
- **DO NOT** silently clamp on overflow (e.g., `i64::MAX + 1 → i64::MAX`). Always
  return `InvalidCoercion` — the user must use a wider type or explicit CAST.
- **DO NOT** produce `Value::Real(f64::NAN)` from any coercion path. If a text
  parses to NaN (e.g., `"NaN"`), return `InvalidCoercion`.
- **DO NOT** implement `Timestamp → Date` truncation in this phase — it's not in scope
  and no caller needs it yet.
- **DO NOT** `unwrap()` or `expect()` in any `src/` code. Every fallible operation
  uses `?` or explicit error construction.
- **DO NOT** add a `coerce_numeric` compatibility shim after removing it — remove it
  cleanly and fix all call sites.

---

## Risks

| Risk | Mitigation |
|---|---|
| `parse_text_to_decimal` has complex edge cases (negative zero, leading zeros in fraction) | Test `"-0.5"`, `"0.00"`, `"007"`, `".5"` explicitly |
| `Int → Decimal` scale depends on the Decimal operand — easy to get wrong in `coerce_for_op` | Test `(Int(5), Decimal(314, 2))` → `(Decimal(500, 2), Decimal(314, 2))` and verify arithmetic gives `8.14` |
| Permissive `"abc"→0` mimics MySQL but may surprise users | Document clearly in module-level doc comment and in error hint |
| Removing `coerce_numeric` may break `eval.rs` tests | Run `cargo test -p nexusdb-sql` after step 9 before committing |
