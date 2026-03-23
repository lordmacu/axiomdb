# Spec: 4.18b — Type Coercion Matrix

## What to build (not how)

A standalone `coerce` module in `axiomdb-types` that converts a `Value` to a
target `DataType`, applying either strict or permissive rules. This module is
used by the expression evaluator (for implicit widening in arithmetic and
comparisons) and will be used by the executor (for column assignment on
INSERT/UPDATE).

The module replaces the private `coerce_numeric` and `coerce_for_compare`
functions currently in `axiomdb-sql/src/eval.rs` with a clean, testable,
re-exportable API.

---

## Inputs / Outputs

### `coerce(value, target, mode) -> Result<Value, DbError>`

Converts `value` to match `target` type.

- **Input:** `value: Value`, `target: DataType`, `mode: CoercionMode`
- **Output:** `Ok(Value)` — the converted value, same variant as `target`
- **Errors:**
  - `DbError::InvalidCoercion` — conversion requested but not valid (strict mode
    for text with non-numeric suffix, or fundamentally incompatible types)

If `value` is already the correct variant for `target`, returns it unchanged
(identity — no allocation).

If `value` is `Value::Null`, always returns `Value::Null` unchanged (NULL
propagates through coercion).

### `coerce_for_op(l, r) -> Result<(Value, Value), DbError>`

Implicit widening between two operands for use in binary arithmetic and
comparison. Returns both values promoted to a common type using the widening
lattice. Replaces `coerce_numeric` in `eval.rs`.

- **Input:** `l: Value`, `r: Value`
- **Output:** `Ok((Value, Value))` — both in the same promoted type
- **Errors:** `DbError::InvalidCoercion` if the pair cannot be promoted
  (e.g., `Text` with `Date` in arithmetic)

### `CoercionMode`

```rust
pub enum CoercionMode {
    /// AxiomDB default. Rejects any Text→numeric conversion where the string
    /// contains non-numeric characters after parsing. '42abc' → error.
    Strict,

    /// MySQL-compatible lenient mode. '42abc' → 42 (strips non-numeric suffix).
    /// Used when the session has SET AXIOM_COMPAT = 'mysql'.
    Permissive,
}
```

---

## Full coercion matrix for `coerce(value, target, mode)`

Rows = `value` variant. Columns = `target` DataType.
`✓` = identity (no-op). `→` = conversion. `E` = error.

| From ↓ \ To → | BOOL | INT | BIGINT | REAL | DECIMAL | TEXT | BYTES | DATE | TIMESTAMP | UUID |
|---|---|---|---|---|---|---|---|---|---|---|
| NULL | Null | Null | Null | Null | Null | Null | Null | Null | Null | Null |
| BOOL | ✓ | perm:0/1, strict:E | perm:0/1, strict:E | perm:0.0/1.0, strict:E | E | E | E | E | E | E |
| INT | perm:0→false/else→true, strict:E | ✓ | widen | widen | widen | E | E | E | E | E |
| BIGINT | E | narrow (check range) | ✓ | widen | widen | E | E | E | E | E |
| REAL | E | E | E | ✓ | E | E | E | E | E | E |
| DECIMAL | E | E | E | E | ✓ | E | E | E | E | E |
| TEXT | E | parse_int | parse_int | parse_float | parse_decimal | ✓ | E | E | E | E |
| BYTES | E | E | E | E | E | E | ✓ | E | E | E |
| DATE | E | E | E | E | E | E | E | ✓ | midnight UTC | E |
| TIMESTAMP | E | E | E | E | E | E | E | truncate | ✓ | E |
| UUID | E | E | E | E | E | E | E | E | E | ✓ |

### Conversion rules in detail

#### Text → Int / BigInt

Strict mode:
- Strip leading/trailing ASCII whitespace.
- Parse the entire remaining string as a decimal integer (optional leading `-`).
- If any character after the optional sign is not `0–9`: `InvalidCoercion`.
- If the parsed value overflows the target type: `InvalidCoercion`.

Permissive mode (MySQL behavior):
- Strip leading/trailing ASCII whitespace.
- Parse as many leading `0–9` (and optional leading `-`) as possible.
- Stop at the first non-digit character.
- If zero digits were consumed (e.g., `"abc"`): result is `0` (MySQL behavior).
- If overflow: `InvalidCoercion` even in permissive mode (not silently clamped).

Examples:

| Input | Target | Strict | Permissive |
|---|---|---|---|
| `"42"` | INT | `Int(42)` | `Int(42)` |
| `"  42  "` | INT | `Int(42)` | `Int(42)` |
| `"-7"` | INT | `Int(-7)` | `Int(-7)` |
| `"42abc"` | INT | `InvalidCoercion` | `Int(42)` |
| `"abc"` | INT | `InvalidCoercion` | `Int(0)` |
| `"99999999999"` | INT | `InvalidCoercion` (overflow) | `InvalidCoercion` |
| `"99999999999"` | BIGINT | `BigInt(99999999999)` | `BigInt(99999999999)` |

#### Text → Real

- Strip leading/trailing whitespace.
- Parse the entire string as an IEEE 754 double (`f64::from_str`).
- In strict mode: any parse failure → `InvalidCoercion`.
- In permissive mode: parse as many characters as form a valid float prefix; if
  none, result is `Real(0.0)`.
- `"NaN"` and `"inf"` → `InvalidCoercion` in both modes (NaN is forbidden in
  stored values).

#### Text → Decimal

- Strip whitespace.
- Parse as `<integer>[.<fraction>]` with optional leading `-`.
- Scale = number of digits in the fraction part.
- Mantissa = integer part × 10^scale + fraction digits.
- Strict: full string must parse; no trailing characters.
- Permissive: stop at first non-numeric character after decimal point.
- Examples: `"3.14"` → `Decimal(314, 2)`, `"100"` → `Decimal(100, 0)`.

#### Int → BigInt (widening, lossless)

`Int(n)` → `BigInt(n as i64)` — always succeeds, no information loss.

#### BigInt → Int (narrowing)

`BigInt(n)` → `Int(n as i32)` if `n` fits in `[i32::MIN, i32::MAX]`.
Otherwise `InvalidCoercion { reason: "value {n} overflows INT range" }`.

Used when a BigInt literal is stored into an INT column.

#### Int → Real, BigInt → Real (widening, potential precision loss)

Always succeeds. For values larger than 2^53, the f64 representation may not
be exact, but this is the standard SQL behavior and is not an error.

#### Int → Decimal, BigInt → Decimal (widening)

`Int(n)` → `Decimal(n as i128, 0)`. `BigInt(n)` → `Decimal(n as i128, 0)`.
Always succeeds.

#### Date → Timestamp (midnight UTC)

`Date(days)` → `Timestamp(days as i64 * 86_400_000_000)`.

`86_400_000_000` = 86400 seconds × 1_000_000 microseconds/second.

Days can be negative (before 1970-01-01). The multiplication uses `i64::checked_mul`
and returns `InvalidCoercion` on overflow (which cannot happen for plausible dates
but must be handled for correctness).

#### Bool → Int / BigInt / Real (permissive mode only)

`Bool(true)` → `Int(1)`, `Bool(false)` → `Int(0)`.
`Bool(true)` → `BigInt(1)`, `Bool(false)` → `BigInt(0)`.
`Bool(true)` → `Real(1.0)`, `Bool(false)` → `Real(0.0)`.

In strict mode: `InvalidCoercion` (AxiomDB does not implicitly treat booleans
as integers).

#### All other combinations

`InvalidCoercion { from: value.variant_name(), to: target.name(), value: value.to_string(), reason: "no implicit conversion" }`.

---

## `coerce_for_op` widening lattice

For use in `eval_binary` (arithmetic and comparison). Both operands are widened
to the same type using this lattice:

```
Bool < Int < BigInt < Real
                    < Decimal
```

Rules:
- Same type: no-op.
- `Bool` + any numeric (strict mode): `InvalidCoercion` — booleans don't participate in arithmetic.
- `Int` + `BigInt`: both → `BigInt`.
- `Int` + `Real`: both → `Real`.
- `BigInt` + `Real`: both → `Real`.
- `Int` + `Decimal`: `Int` → `Decimal` (scale 0), keep `Decimal`.
- `BigInt` + `Decimal`: `BigInt` → `Decimal` (scale 0), keep `Decimal`.
- `Real` + `Decimal`: `InvalidCoercion` — mixing floating-point and exact decimal
  is never implicit (requires explicit CAST).
- Any non-numeric + any numeric: `InvalidCoercion`.
- `Text` in arithmetic: `InvalidCoercion` (implicit parse not attempted at op time;
  the executor's column assignment is where Text→numeric coercion is applied).

---

## New error variant

Add to `axiomdb-core/src/error.rs`:

```rust
#[error("cannot coerce {value} ({from}) to {to}: {reason}")]
InvalidCoercion {
    from:   String,   // source type name, e.g. "Text"
    to:     String,   // target type name, e.g. "INT"
    value:  String,   // Display of the value, e.g. "'42abc'"
    reason: String,   // human-readable explanation
}
```

SQLSTATE: `22018` (invalid_character_value_for_cast). Map in `sqlstate()`.

---

## Use cases

### 1. Happy path — numeric widening in arithmetic

```sql
SELECT 1 + 9999999999  -- Int + BigInt → coerce_for_op → both BigInt → BigInt(10000000000)
```

### 2. Text literal into INT column (executor INSERT, Phase 4.5)

```sql
CREATE TABLE t (id INT);
INSERT INTO t VALUES ('42');  -- coerce('42', DataType::Int, Strict) → Int(42)
```

### 3. Text with garbage in strict mode

```sql
INSERT INTO t VALUES ('42abc');  -- Strict → InvalidCoercion
```

### 4. Text with garbage in permissive mode

```sql
SET AXIOM_COMPAT = 'mysql';
INSERT INTO t VALUES ('42abc');  -- Permissive → Int(42)
```

### 5. Date used in timestamp context

```sql
SELECT * FROM events WHERE created_at > '2026-01-01';
-- After date parsing (Phase 4.19): Date(20454) → coerce → Timestamp(20454 * 86_400_000_000)
```

### 6. NULL always passes through

```sql
INSERT INTO t (age) VALUES (NULL);  -- coerce(Null, DataType::Int, Strict) → Null
```

### 7. Overflow in narrowing

```sql
-- If executor stores BigInt(999999999999) into INT column:
-- coerce(BigInt(999999999999), DataType::Int, Strict) → InvalidCoercion
```

---

## Acceptance criteria

- [ ] `axiomdb-types/src/coerce.rs` compiles with `cargo check`
- [ ] `coerce(value, target, Strict)` passes the full matrix for all valid conversions
- [ ] `coerce(value, target, Strict)` returns `InvalidCoercion` for all `E` cells
- [ ] `coerce(value, target, Permissive)` applies MySQL lenient rules for Text→numeric
- [ ] `coerce(Value::Null, any, any)` always returns `Ok(Value::Null)`
- [ ] `coerce_for_op` replaces `coerce_numeric` in `eval.rs` with no change in behavior
  for the cases it previously handled (Int/BigInt/Real/Decimal widening)
- [ ] `coerce_for_op` returns `InvalidCoercion` for previously-unreachable combinations
  (Text+numeric, Date+anything)
- [ ] `DbError::InvalidCoercion` variant added to `axiomdb-core/src/error.rs`
- [ ] `DbError::sqlstate()` maps `InvalidCoercion` → `"22018"`
- [ ] Unit tests: at least 2 tests per conversion rule, including overflow and NULL
- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace -- -D warnings` passes
- [ ] `cargo fmt --check` passes
- [ ] No `unwrap()` in `src/` outside tests

---

## Out of scope

- Text → Date / Timestamp parsing (requires chrono — Phase 4.19)
- Text → UUID parsing (Phase 4.19)
- Text → Bool (`'true'→BOOL`) — not needed until 4.19
- Timestamp → Date truncation — not needed until executor handles ORDER BY / GROUP BY on mixed temporal types
- Decimal → Real explicit conversion (only via CAST, Phase 4.12b)
- The `SET AXIOM_COMPAT` session variable — Phase 5. Until then, `CoercionMode::Strict` is always used from application code; tests will call both modes directly.
- Real → Int narrowing (lossy — requires explicit CAST per SQL standard)

---

## Dependencies

- `axiomdb-types` — houses the new module; already has `Value` and `DataType`
- `axiomdb-core` — add `DbError::InvalidCoercion`
- `axiomdb-sql/src/eval.rs` — replace `coerce_numeric` / `coerce_for_compare` with calls to `coerce_for_op`

No new crate dependencies required.
