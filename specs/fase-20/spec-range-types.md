# Spec: range-types

Phase: 20 — Types + import/export
Task: Range types — int4range, int8range, numrange, daterange, tsrange
Status: approved

## Context

Range types are first-class PostgreSQL types that represent a contiguous span of values
with configurable inclusivity on each bound. AxiomDB already has `DataType::Array(Box<DataType>)`
(Phase 20.4) as the pattern for parameterized column types. Range types follow the same pattern
and extend `Value`, `DataType`, `ColumnType`, and the row codec with a new variant.
This is Phase 20's last major type addition before Phase 24 (advanced analytics).

## Goal

Implement five built-in range types (`int4range`, `int8range`, `numrange`, `daterange`,
`tsrange`) with constructor functions, cast syntax, containment/overlap operators, and
set-arithmetic operators.

## Non-goals

- `GiST` index support for range overlap queries — deferred to Phase 30.2
- Multi-range types (`int4multirange`, etc.) — deferred to Phase 24.11
- User-defined range types (`CREATE TYPE myrange AS RANGE (...)`) — deferred
- `REPEATABLE` or seeded range constructors
- `tstzrange` (timestamptz range) — deferred; AxiomDB has no TIMESTAMPTZ yet
- `numrange` subdiff / `tsrange` subdiff (used by GiST internals only)

## Behavior

### Types supported

| SQL name | Inner DataType | Element type |
|---|---|---|
| `INT4RANGE` | `DataType::Int` | 32-bit integer |
| `INT8RANGE` | `DataType::BigInt` | 64-bit integer |
| `NUMRANGE` | `DataType::Decimal` | numeric |
| `DATERANGE` | `DataType::Date` | date (days since epoch) |
| `TSRANGE` | `DataType::Timestamp` | timestamp (µs since epoch) |

### Public API — axiomdb-types

```rust
/// A range value with optional lower/upper bounds.
///
/// Bound semantics follow PostgreSQL:
///   `[` lower_inc=true   `(` lower_inc=false
///   `]` upper_inc=true   `)` upper_inc=false
///   lower=None → unbounded below (-∞)
///   upper=None → unbounded above (+∞)
///   is_empty=true → the canonical empty range (no points)
///
/// Invariant: if is_empty=true, lower=None, upper=None, lower_inc=false, upper_inc=false.
#[derive(Debug, Clone, PartialEq)]
pub struct RangeValue {
    pub lower: Option<Value>,
    pub upper: Option<Value>,
    pub lower_inc: bool,
    pub upper_inc: bool,
    pub is_empty: bool,
}

impl RangeValue {
    /// Creates the canonical empty range.
    pub fn empty() -> Self { ... }

    /// Creates a finite range. Returns `DbError::InvalidValue` if the range
    /// is inverted (lower > upper with both bounds present and comparable).
    pub fn new(
        lower: Option<Value>,
        upper: Option<Value>,
        lower_inc: bool,
        upper_inc: bool,
    ) -> Result<Self, DbError> { ... }

    /// True if `point` is within this range.
    pub fn contains_value(&self, point: &Value) -> bool { ... }

    /// True if this range and `other` share at least one point.
    pub fn overlaps(&self, other: &RangeValue) -> bool { ... }

    /// Union of two adjacent or overlapping ranges. Returns `None` if they
    /// are disjoint and non-adjacent (union would be non-contiguous).
    pub fn union(&self, other: &RangeValue) -> Option<RangeValue> { ... }

    /// Intersection of two ranges. Returns the empty range if disjoint.
    pub fn intersection(&self, other: &RangeValue) -> RangeValue { ... }

    /// Difference: points in self but not in other.
    /// Returns `None` when the result would be non-contiguous
    /// (other is strictly interior to self).
    pub fn difference(&self, other: &RangeValue) -> Option<RangeValue> { ... }
}
```

**`Value` extension:**
```rust
// Added to the Value enum in axiomdb-types/src/value.rs
Value::Range(Box<RangeValue>),
```

**`DataType` extension:**
```rust
// Added to DataType in axiomdb-types/src/types.rs
DataType::Range(Box<DataType>),
// name(): "INT4RANGE" for DataType::Range(Box::new(DataType::Int)), etc.
```

**`ColumnType` extension:**
```rust
// Added to ColumnType in axiomdb-catalog/src/schema_database.rs
ColumnType::Range = 14,
```

### Public API — axiomdb-sql: constructor function

```sql
int4range(lower, upper [, bounds])   -- bounds default: '[)'
int8range(lower, upper [, bounds])
numrange(lower, upper [, bounds])
daterange(lower, upper [, bounds])
tsrange(lower, upper [, bounds])
```

- `lower` and `upper` are nullable expressions of the matching element type; `NULL` = unbounded.
- `bounds` is an optional string literal: `'()'`, `'[)'`, `'(]'`, `'[]'`. Default: `'[)'`.
- Returns `Value::Range(...)`.
- Error: `DbError::InvalidValue` if bounds string is not one of the 4 valid values.
- Error: `DbError::InvalidValue` if lower > upper and the resulting range is non-empty.

### Public API — axiomdb-sql: cast syntax

```sql
'[1,10)'::int4range
'empty'::int4range
'(,5]'::int8range       -- unbounded lower
'[2020-01-01,)'::daterange  -- unbounded upper
```

The cast evaluator parses the text literal into a `RangeValue` using PostgreSQL's
canonical format: `[lower,upper)` where `lower` and `upper` are type-formatted values
or empty string for unbounded.

### Public API — axiomdb-sql: operators (evaluator dispatch)

All operators dispatch based on the runtime type of operands (no new AST nodes). The
evaluator's `eval_binary` function gains Range arms inside existing BinaryOp handlers:

| SQL | BinaryOp used | Semantics |
|---|---|---|
| `r @> elem` | `JsonContains` | range contains element |
| `r @> r2` | `JsonContains` | range contains range |
| `elem <@ r` | `JsonContainedBy` | element is in range |
| `r <@ r2` | `JsonContainedBy` | range is contained by range |
| `r1 && r2` | `ArrayOverlap` | ranges overlap |
| `r1 + r2` | `Add` | range union (error if disjoint) |
| `r1 * r2` | `Mul` | range intersection |
| `r1 - r2` | `Sub` | range difference (error if non-contiguous result) |

Comparison operators `=`, `<>`, `<`, `>`, `<=`, `>=` work via the existing
`BinaryOp::Eq/Ne/Lt/Gt/Le/Ge` evaluator with a `Value::Range` arm.

Range ordering: empty < all others; otherwise compare lower bounds first (unbounded lower
= −∞), then upper bounds (unbounded upper = +∞), with tie-breaking on inclusivity
(inclusive bound > exclusive bound for lower; inclusive > exclusive for upper).

### Public API — axiomdb-sql: scalar functions

```sql
lower(r)        → element type or NULL (unbounded lower)
upper(r)        → element type or NULL (unbounded upper)
isempty(r)      → BOOL
lower_inc(r)    → BOOL  (false if empty or unbounded)
upper_inc(r)    → BOOL  (false if empty or unbounded)
lower_inf(r)    → BOOL  (true if unbounded lower)
upper_inf(r)    → BOOL  (true if unbounded upper)
```

### Semantics

**Canonical empty range:** `isempty(int4range(1, 1, '()'))` → true (adjacent exclusive bounds with nothing between them for discrete types). For continuous types (`numrange`, `tsrange`), `numrange(1.5, 1.5, '()')` is also empty.

**Integer canonicalization:** `int4range` and `int8range` are **discrete** types. The upper bound is canonicalized to exclusive form: `[1,5]` → `[1,6)`. This ensures `[1,5)` and `[1,4]` are equal. Constructor and cast both canonicalize on creation.

**Non-integer types** (`numrange`, `daterange`, `tsrange`) are **continuous** — no canonicalization.

**`daterange` canonicalization:** Like `int4range`, dates are discrete. `[2024-01-01, 2024-01-05]` → `[2024-01-01, 2024-01-06)`.

**Union error:** `r1 + r2` returns `DbError::InvalidValue` if the ranges are disjoint and non-adjacent (result would be non-contiguous).

**Difference error:** `r1 - r2` returns `DbError::InvalidValue` if `r2` is strictly interior to `r1` (result would be non-contiguous).

**NULL propagation:** Any operator with a `NULL` operand returns `NULL` (following standard SQL rules). Exception: `isempty(NULL)` → `NULL` (not false).

### Error cases

| Input | Expected error | Notes |
|---|---|---|
| `int4range(10, 1)` | `DbError::InvalidValue` | inverted range |
| `int4range(1, 1, '()')` | OK (empty) | empty is valid |
| `int4range(1, 1, '(]')` | empty (discrete canon.) | canonical empty |
| `'[1,10)'::int5range` | `DbError::ParseError` | unknown type |
| `'bad'::int4range` | `DbError::ParseError` | malformed literal |
| `int4range(1, 5, '<<')` | `DbError::InvalidValue` | invalid bounds string |
| `r1 + r2` disjoint | `DbError::InvalidValue` | non-contiguous union |
| `r1 - r2` r2 interior to r1 | `DbError::InvalidValue` | non-contiguous diff |

## On-disk format

Range values are stored inline in the row codec, immediately after any preceding fields:

```
Byte layout for Value::Range:
  offset  size   field        description
  0       1      flags        bit 0 = EMPTY
                              bit 1 = LOWER_BOUNDED (0 = unbounded)
                              bit 2 = UPPER_BOUNDED (0 = unbounded)
                              bit 3 = LOWER_INC
                              bit 4 = UPPER_INC
  1       var    lower_value  present only if LOWER_BOUNDED; encoded as the
                              element type's native codec (4 bytes for Int,
                              8 bytes for BigInt/Timestamp, 4 bytes for Date,
                              17 bytes for Decimal)
  1+lo    var    upper_value  present only if UPPER_BOUNDED; same encoding
```

Compatibility rule: the flags byte is versioned by bit 7 (currently 0). Future
extensions that change encoding set bit 7 = 1, which existing decoders will reject
with `DbError::ParseError { message: "unsupported range encoding version" }`.

## Edge cases

- [ ] Empty range: `int4range(3, 3, '()')`, `'empty'::int4range`
- [ ] Unbounded lower: `int4range(NULL, 5)` or `'(,5]'::int4range`
- [ ] Unbounded upper: `int4range(1, NULL)` or `'[1,)'::int4range`
- [ ] Fully unbounded: `int4range(NULL, NULL)` = `'(,)'::int4range`
- [ ] NULL range value in a row → stored as null bitmap entry (no range bytes)
- [ ] NULL operand in operator → result is NULL
- [ ] Discrete canonicalization: `[1,5]` → `[1,6)` for int4range
- [ ] `daterange` canonicalization: `[2024-01-01, 2024-01-05]` → `[2024-01-01, 2024-01-06)`
- [ ] `numrange` non-discrete: `[1.0, 2.0]` stays as-is
- [ ] Adjacent ranges union: `[1,5) + [5,10)` → `[1,10)` (touching at boundary)
- [ ] Disjoint union error: `[1,3) + [5,10)` → `DbError::InvalidValue`
- [ ] Intersection of disjoint ranges → empty range
- [ ] `r @> r` (self-containment) → true for non-empty ranges
- [ ] Comparison: `[1,5) < [1,6)` (same lower, upper decides)
- [ ] Comparison: empty range is less than any non-empty range

## Performance budget

No specific throughput requirement for Phase 20.13. Range operations are O(1) in all
cases (no iteration). Codec encode/decode for a bounded range: ≤ 5 bytes overhead.

## Dependencies

- Depends on: `axiomdb-types` (Value enum), `axiomdb-catalog` (ColumnType),
  `axiomdb-sql` (parser, evaluator)
- Blocks: Phase 30.2 GiST indexes (uses range overlap in index operations)

## Open questions

All resolved during brainstorm.

## Done criteria

- [ ] `Value::Range`, `DataType::Range`, `ColumnType::Range = 14` exist
- [ ] Codec round-trip for all 5 range types (bounded, unbounded, empty)
- [ ] `CREATE TABLE t (r INT4RANGE)` parses and executes
- [ ] `INSERT INTO t VALUES (int4range(1, 10))` inserts a range value
- [ ] `'[1,10)'::int4range` cast produces the correct `Value::Range`
- [ ] `r @> 5` / `r @> r2` containment returns correct bool
- [ ] `r1 && r2` overlap returns correct bool
- [ ] `r1 + r2` union, `r1 * r2` intersection, `r1 - r2` difference work
- [ ] `lower()`, `upper()`, `isempty()`, `lower_inc()`, `upper_inc()`,
      `lower_inf()`, `upper_inf()` functions return correct values
- [ ] `=`, `<>`, `<`, `>`, `<=`, `>=` comparisons work
- [ ] All 5 type names work: `int4range`, `int8range`, `numrange`, `daterange`, `tsrange`
- [ ] Integer canonicalization: `[1,5]` → `[1,6)` for int4/int8range
- [ ] `daterange` canonicalization applies
- [ ] Disjoint union / interior difference return `DbError::InvalidValue`
- [ ] All edge cases above have integration tests
- [ ] `cargo nextest run --workspace` passes
- [ ] `cargo clippy --workspace -- -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] 3+ wire assertions pass

## References

- PostgreSQL range type source: `research/postgres/src/backend/utils/adt/rangetypes.c`
- PostgreSQL range type header: `research/postgres/src/include/utils/rangetypes.h`
- Array type pattern (Phase 20.4): `specs/fase-20/spec-20.4-arrays.md`
- Phase 20 doc: `docs/progreso.md` line 223
- SQL:2011 §4.6 (range types)
