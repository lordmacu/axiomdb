# Plan: range-types

Phase: 20 — Types + import/export
Task: Range types — int4range, int8range, numrange, daterange, tsrange
Spec: specs/fase-20/spec-range-types.md
Status: in-progress

## Summary

Three steps, all building on each other. Step 1 adds the `RangeValue` type and all
range arithmetic in `axiomdb-types` plus `ColumnType::Range = 14` in the catalog —
the pure data layer with no SQL dependencies. Step 2 wires the type into the parser
and DDL pipeline (type names, constructor functions, cast syntax, table.rs mappings).
Step 3 adds the evaluator operators and scalar functions, writes the full integration
test suite, and closes the subphase. The key implementation detail is that all operators
(`@>`, `&&`, `+`, `*`, `-`, comparisons) dispatch at runtime inside existing `BinaryOp`
handlers based on `Value::Range` arms — no new AST nodes needed.

## Dependencies

Must be done first:
- [x] spec-range-types.md approved

Blocks (until this plan is done):
- [ ] Phase 30.2 GiST indexes (range overlap in index operations)

## Affected files

New files:
- `crates/axiomdb-types/src/range_value.rs` — `RangeValue` struct + all range methods
- `crates/axiomdb-sql/src/eval/functions/range.rs` — scalar functions lower/upper/isempty/etc.
- `crates/axiomdb-sql/tests/integration_range_types.rs` — ~25 integration tests

Modified files:
- `crates/axiomdb-types/src/value.rs` — add `Range(Box<RangeValue>)` variant
- `crates/axiomdb-types/src/types.rs` — add `DataType::Range(Box<DataType>)` variant
- `crates/axiomdb-types/src/codec.rs` — encode/decode Range values
- `crates/axiomdb-types/src/coerce.rs` — Range type compatibility
- `crates/axiomdb-types/src/lib.rs` — re-export `RangeValue`
- `crates/axiomdb-catalog/src/schema_database.rs` — `ColumnType::Range = 14`
- `crates/axiomdb-sql/src/table.rs` — `ColumnType::Range ↔ DataType::Range` mappings (4 sites)
- `crates/axiomdb-sql/src/values_clause.rs` — Range in column type + value detection
- `crates/axiomdb-sql/src/parser/ddl.rs` — `INT4RANGE`/`INT8RANGE`/`NUMRANGE`/`DATERANGE`/`TSRANGE` type names
- `crates/axiomdb-sql/src/parser/expr.rs` — `int4range(...)` constructor + `'...'::int4range` cast
- `crates/axiomdb-sql/src/eval/ops.rs` — Range arms in `eval_binary`
- `crates/axiomdb-sql/src/eval/functions/mod.rs` — register range functions + fix lower/upper collision
- `crates/axiomdb-sql/src/expr_to_sql.rs` — Display for Range (needed by EXPLAIN)
- `crates/axiomdb-network/src/mysql/` — wire encoding for Range values

---

## Step 1 — Types layer: RangeValue + codec + ColumnType

**Goal:** `RangeValue` struct with full range arithmetic, wired into `Value`, `DataType`, `ColumnType`, and the codec.

**Files:**
- NEW: `crates/axiomdb-types/src/range_value.rs`
- MODIFIED: `crates/axiomdb-types/src/value.rs`
- MODIFIED: `crates/axiomdb-types/src/types.rs`
- MODIFIED: `crates/axiomdb-types/src/codec.rs`
- MODIFIED: `crates/axiomdb-types/src/coerce.rs`
- MODIFIED: `crates/axiomdb-types/src/lib.rs`
- MODIFIED: `crates/axiomdb-catalog/src/schema_database.rs`

**Approach:** TDD — unit tests for `RangeValue` methods first, then codec round-trip tests.

### Tests to add

```rust
// crates/axiomdb-types/src/range_value.rs (unit tests at bottom)

#[test]
fn range_empty_construction() {
    let r = RangeValue::empty();
    assert!(r.is_empty);
    assert!(!r.lower_inc);
}

#[test]
fn range_int4_canonicalization() {
    // [1,5] with Int → canonicalize upper to exclusive → [1,6)
    let r = RangeValue::new_discrete(
        Some(Value::Int(1)), Some(Value::Int(5)), true, true,
    ).unwrap();
    assert_eq!(r.upper, Some(Value::Int(6)));
    assert!(!r.upper_inc);
}

#[test]
fn range_contains_value() {
    let r = RangeValue::new(Some(Value::Int(1)), Some(Value::Int(10)), true, false).unwrap();
    assert!(r.contains_value(&Value::Int(5)));
    assert!(!r.contains_value(&Value::Int(10)));  // exclusive upper
    assert!(!r.contains_value(&Value::Int(0)));
}

#[test]
fn range_overlaps() {
    let a = RangeValue::new(Some(Value::Int(1)), Some(Value::Int(5)), true, false).unwrap();
    let b = RangeValue::new(Some(Value::Int(3)), Some(Value::Int(8)), true, false).unwrap();
    let c = RangeValue::new(Some(Value::Int(6)), Some(Value::Int(9)), true, false).unwrap();
    assert!(a.overlaps(&b));
    assert!(!a.overlaps(&c));
}

#[test]
fn range_union_adjacent() {
    let a = RangeValue::new(Some(Value::Int(1)), Some(Value::Int(5)), true, false).unwrap();
    let b = RangeValue::new(Some(Value::Int(5)), Some(Value::Int(10)), true, false).unwrap();
    let u = a.union(&b).unwrap();
    assert_eq!(u.lower, Some(Value::Int(1)));
    assert_eq!(u.upper, Some(Value::Int(10)));
}

#[test]
fn range_union_disjoint_errors() {
    let a = RangeValue::new(Some(Value::Int(1)), Some(Value::Int(3)), true, false).unwrap();
    let b = RangeValue::new(Some(Value::Int(5)), Some(Value::Int(8)), true, false).unwrap();
    assert!(a.union(&b).is_none());  // non-contiguous → None
}

#[test]
fn range_intersection_disjoint_empty() {
    let a = RangeValue::new(Some(Value::Int(1)), Some(Value::Int(3)), true, false).unwrap();
    let b = RangeValue::new(Some(Value::Int(5)), Some(Value::Int(8)), true, false).unwrap();
    let i = a.intersection(&b);
    assert!(i.is_empty);
}

#[test]
fn range_codec_roundtrip_bounded() {
    use axiomdb_types::{codec::{encode_row, decode_row}, types::DataType};
    let r = RangeValue::new(Some(Value::Int(1)), Some(Value::Int(10)), true, false).unwrap();
    let schema = vec![DataType::Range(Box::new(DataType::Int))];
    let values = vec![Value::Range(Box::new(r.clone()))];
    let encoded = encode_row(&values, &schema).unwrap();
    let decoded = decode_row(&encoded, &schema).unwrap();
    assert_eq!(decoded[0], Value::Range(Box::new(r)));
}

#[test]
fn range_codec_roundtrip_unbounded() {
    use axiomdb_types::{codec::{encode_row, decode_row}, types::DataType};
    let r = RangeValue::new(None, Some(Value::Int(10)), false, true).unwrap();
    let schema = vec![DataType::Range(Box::new(DataType::Int))];
    let values = vec![Value::Range(Box::new(r.clone()))];
    let encoded = encode_row(&values, &schema).unwrap();
    let decoded = decode_row(&encoded, &schema).unwrap();
    assert_eq!(decoded[0], Value::Range(Box::new(r)));
}

#[test]
fn range_codec_roundtrip_empty() {
    use axiomdb_types::{codec::{encode_row, decode_row}, types::DataType};
    let r = RangeValue::empty();
    let schema = vec![DataType::Range(Box::new(DataType::Int))];
    let values = vec![Value::Range(Box::new(r.clone()))];
    let encoded = encode_row(&values, &schema).unwrap();
    let decoded = decode_row(&encoded, &schema).unwrap();
    assert_eq!(decoded[0], Value::Range(Box::new(r)));
}
```

### Implementation outline

```rust
// crates/axiomdb-types/src/range_value.rs

// Codec flags byte
const RANGE_EMPTY: u8 = 0x01;
const RANGE_LOWER_BOUNDED: u8 = 0x02;
const RANGE_UPPER_BOUNDED: u8 = 0x04;
const RANGE_LOWER_INC: u8 = 0x08;
const RANGE_UPPER_INC: u8 = 0x10;

#[derive(Debug, Clone, PartialEq)]
pub struct RangeValue {
    pub lower: Option<Value>,
    pub upper: Option<Value>,
    pub lower_inc: bool,
    pub upper_inc: bool,
    pub is_empty: bool,
}

impl RangeValue {
    pub fn empty() -> Self { ... }

    // For continuous types (numrange, tsrange): no canonicalization
    pub fn new(lower: Option<Value>, upper: Option<Value>,
               lower_inc: bool, upper_inc: bool) -> Result<Self, DbError> {
        // validate: if both bounded, lower <= upper (or lower == upper only
        // with both inclusive)
    }

    // For discrete types (int4range, int8range, daterange): canonicalize upper
    // to exclusive form.
    pub fn new_discrete(lower: Option<Value>, upper: Option<Value>,
                        lower_inc: bool, upper_inc: bool) -> Result<Self, DbError> {
        // if upper_inc: increment upper by 1, set upper_inc=false
    }

    pub fn contains_value(&self, point: &Value) -> bool { ... }
    pub fn overlaps(&self, other: &RangeValue) -> bool { ... }
    pub fn union(&self, other: &RangeValue) -> Option<RangeValue> { ... }
    pub fn intersection(&self, other: &RangeValue) -> RangeValue { ... }
    pub fn difference(&self, other: &RangeValue) -> Option<RangeValue> { ... }
}
```

```rust
// In value.rs: add to Value enum
/// SQL range value (Phase 20.13).
Range(Box<RangeValue>),
```

```rust
// In types.rs: add to DataType enum
/// SQL range type (Phase 20.13). Inner type is Int/BigInt/Decimal/Date/Timestamp.
Range(Box<DataType>),

// In DataType::name():
Self::Range(inner) => {
    let inner_name = inner.name().to_lowercase();
    match inner_name.as_str() {
        "int" => "INT4RANGE".into(),
        "bigint" => "INT8RANGE".into(),
        "decimal" => "NUMRANGE".into(),
        "date" => "DATERANGE".into(),
        "timestamp" => "TSRANGE".into(),
        _ => format!("RANGE({})", inner.name()),
    }
}
```

```rust
// In codec.rs: encode Range
Value::Range(rv) => {
    if let DataType::Range(_elem_dt) = &schema[i] {
        encode_range_value(&mut buf, rv, _elem_dt)?;
    } else {
        return Err(DbError::TypeMismatch { ... });
    }
}

// encode_range_value:
fn encode_range_value(buf: &mut Vec<u8>, rv: &RangeValue, elem_dt: &DataType) -> Result<(), DbError> {
    let mut flags: u8 = 0;
    if rv.is_empty { flags |= RANGE_EMPTY; buf.push(flags); return Ok(()); }
    if rv.lower.is_some() { flags |= RANGE_LOWER_BOUNDED; }
    if rv.upper.is_some() { flags |= RANGE_UPPER_BOUNDED; }
    if rv.lower_inc       { flags |= RANGE_LOWER_INC; }
    if rv.upper_inc       { flags |= RANGE_UPPER_INC; }
    buf.push(flags);
    if let Some(ref lo) = rv.lower { encode_range_bound(buf, lo, elem_dt)?; }
    if let Some(ref hi) = rv.upper { encode_range_bound(buf, hi, elem_dt)?; }
    Ok(())
}
```

```rust
// In schema_database.rs: add to ColumnType
Range = 14,
// and in TryFrom<u8>: 14 => Ok(Self::Range),
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-types
./tools/vm.sh test -p axiomdb-catalog
./tools/vm.sh clippy
```

### Commit

```
feat(fase-20): step 1 — RangeValue type + codec + ColumnType::Range

Step 1 of specs/fase-20/plan-range-types.md
```

---

## Step 2 — Parser + DDL cascades

**Goal:** `CREATE TABLE t (r INT4RANGE)` and `INSERT INTO t VALUES (int4range(1,10))` work end-to-end.

**Files:**
- MODIFIED: `crates/axiomdb-sql/src/parser/ddl.rs` — range type names
- MODIFIED: `crates/axiomdb-sql/src/parser/expr.rs` — constructor + cast
- MODIFIED: `crates/axiomdb-sql/src/table.rs` — ColumnType::Range ↔ DataType::Range
- MODIFIED: `crates/axiomdb-sql/src/values_clause.rs` — Range in value/type inference
- MODIFIED: `crates/axiomdb-sql/src/expr_to_sql.rs` — Display for Range value

### Tests to add

```rust
// crates/axiomdb-sql/tests/integration_range_types.rs

#[test]
fn range_create_table_int4range() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t_rng (id INT, r INT4RANGE)", &mut storage, &mut txn);
    // Just check it doesn't error — schema created successfully
}

#[test]
fn range_insert_constructor() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t_ins (r INT4RANGE)", &mut storage, &mut txn);
    run("INSERT INTO t_ins VALUES (int4range(1, 10))", &mut storage, &mut txn);
    let res = rows(run("SELECT r FROM t_ins", &mut storage, &mut txn));
    assert_eq!(res.len(), 1);
}

#[test]
fn range_insert_cast_literal() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t_cast (r INT4RANGE)", &mut storage, &mut txn);
    run("INSERT INTO t_cast VALUES ('[1,10)'::int4range)", &mut storage, &mut txn);
    let res = rows(run("SELECT r FROM t_cast", &mut storage, &mut txn));
    assert_eq!(res.len(), 1);
}

#[test]
fn range_insert_empty() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t_empty (r INT4RANGE)", &mut storage, &mut txn);
    run("INSERT INTO t_empty VALUES ('empty'::int4range)", &mut storage, &mut txn);
    let res = rows(run("SELECT isempty(r) FROM t_empty", &mut storage, &mut txn));
    assert_eq!(res[0][0], Value::Bool(true));
}

#[test]
fn range_all_five_types() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t_all (a INT4RANGE, b INT8RANGE, c NUMRANGE, d DATERANGE, e TSRANGE)",
        &mut storage, &mut txn);
}
```

### Implementation outline

```rust
// In parser/ddl.rs — parse_data_type, add cases:
Token::Ident(s) if matches!(s.to_uppercase().as_str(),
    "INT4RANGE" | "INT8RANGE" | "NUMRANGE" | "DATERANGE" | "TSRANGE") => {
    let inner = match s.to_uppercase().as_str() {
        "INT4RANGE" => DataType::Int,
        "INT8RANGE" => DataType::BigInt,
        "NUMRANGE"  => DataType::Decimal,
        "DATERANGE" => DataType::Date,
        "TSRANGE"   => DataType::Timestamp,
        _ => unreachable!(),
    };
    p.advance();
    Ok(DataType::Range(Box::new(inner)))
}
```

```rust
// In parser/expr.rs — parse_primary / function call path: when function name
// is one of the 5 range constructors, parse as Expr::FunctionCall and let
// the evaluator handle it. No AST change needed — the evaluator intercepts by name.

// In cast path ('...'::type_name): when type_name is a range type, produce
// Expr::Cast { expr, data_type: DataType::Range(_) }. The cast evaluator
// calls a parse_range_literal() helper.
```

```rust
// In table.rs — column_type_to_data_type and data_type_to_column_type:
ColumnType::Range => DataType::Range(Box::new(DataType::Int)),  // default (overridden by catalog elem_dt)
DataType::Range(_) => ColumnType::Range,
```

```rust
// In values_clause.rs:
DataType::Range(_) => ColumnType::Range,
Value::Range(_) => DataType::Range(Box::new(DataType::Int)),  // type inferred later
```

```rust
// In expr_to_sql.rs — BinaryOp display for Range won't change (reuses @>/&&).
// Add Value::Range display in format_value helper if it exists.
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql --test integration_range_types
./tools/vm.sh clippy
```

### Commit

```
feat(fase-20): step 2 — parser + DDL for range types

Step 2 of specs/fase-20/plan-range-types.md
```

---

## Step 3 — Operators + functions + close

**Goal:** All operators and scalar functions work; full integration test suite; subphase closed.

**Files:**
- MODIFIED: `crates/axiomdb-sql/src/eval/ops.rs` — Range dispatch arms in `eval_binary`
- NEW: `crates/axiomdb-sql/src/eval/functions/range.rs`
- MODIFIED: `crates/axiomdb-sql/src/eval/functions/mod.rs` — register range.rs + fix lower/upper collision
- MODIFIED: `crates/axiomdb-sql/tests/integration_range_types.rs` — fill out all ~25 tests
- MODIFIED: `tools/wire-test.py` — 3 wire assertions
- MODIFIED: `docs/progreso.md`, `docs-site/...`, `memory/project_state.md`

### Key implementation details

**`eval_binary` dispatch — add Range arms BEFORE the NULL check (line ~256 in ops.rs):**
```rust
// Range operator dispatch (Phase 20.13)
if matches!(&l, Value::Range(_)) || matches!(&r, Value::Range(_)) {
    return eval_binary_range(op, l, r);
}
```

**`eval_binary_range` — new function:**
```rust
fn eval_binary_range(op: BinaryOp, l: Value, r: Value) -> Result<Value, DbError> {
    // NULL propagation
    if matches!(l, Value::Null) || matches!(r, Value::Null) {
        return Ok(Value::Null);
    }
    match op {
        BinaryOp::JsonContains => {  // r @> elem  or  r @> r2
            let lrange = extract_range(&l)?;
            match &r {
                Value::Range(rr) => Ok(Value::Bool(
                    lrange.intersection(rr) == **rr && !rr.is_empty || rr.is_empty
                    // simpler: rrange is contained by lrange iff every point in rrange is in lrange
                )),
                _ => Ok(Value::Bool(lrange.contains_value(&r))),
            }
        }
        BinaryOp::JsonContainedBy => {  // elem <@ r  or  r <@ r2
            // swap and delegate to JsonContains
            eval_binary_range(BinaryOp::JsonContains, r, l)
        }
        BinaryOp::ArrayOverlap => {
            let (a, b) = (extract_range(&l)?, extract_range(&r)?);
            Ok(Value::Bool(a.overlaps(&b)))
        }
        BinaryOp::Add => {
            let (a, b) = (extract_range(&l)?, extract_range(&r)?);
            a.union(&b).map(|u| Value::Range(Box::new(u)))
                .ok_or_else(|| DbError::InvalidValue {
                    message: "range union: disjoint non-adjacent ranges".into()
                })
        }
        BinaryOp::Mul => {
            let (a, b) = (extract_range(&l)?, extract_range(&r)?);
            Ok(Value::Range(Box::new(a.intersection(&b))))
        }
        BinaryOp::Sub => {
            let (a, b) = (extract_range(&l)?, extract_range(&r)?);
            a.difference(&b).map(|d| Value::Range(Box::new(d)))
                .ok_or_else(|| DbError::InvalidValue {
                    message: "range difference: result would be non-contiguous".into()
                })
        }
        BinaryOp::Eq | BinaryOp::NotEq | BinaryOp::Lt |
        BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq => {
            eval_range_comparison(op, l, r)
        }
        _ => Err(DbError::TypeMismatch {
            expected: "range operator".into(),
            got: format!("{op:?} not supported for ranges"),
        }),
    }
}
```

**`lower`/`upper` collision fix in `mod.rs`:**
```rust
// BEFORE the string dispatch arm:
"lower" | "upper" => {
    if args.len() == 1 {
        let val = crate::eval::eval(&args[0], row)?;
        if matches!(&val, Value::Range(_)) {
            return range::eval_range_function(&lower, val);
        }
        // fall through to string UPPER()/LOWER()
        return string::eval(&lower, args, row);
    }
    string::eval(&lower, args, row)
}
```

Note: this must come BEFORE `"upper" | "ucase" | ... | "lower" | "lcase"` in the match to take priority for Range args.

**`eval/functions/range.rs`:**
```rust
pub(super) fn eval_range_function(name: &str, val: Value) -> Result<Value, DbError> {
    let r = extract_range_value(&val)?;
    match name {
        "lower"     => Ok(r.lower.clone().unwrap_or(Value::Null)),
        "upper"     => Ok(r.upper.clone().unwrap_or(Value::Null)),
        "isempty"   => Ok(Value::Bool(r.is_empty)),
        "lower_inc" => Ok(Value::Bool(!r.is_empty && r.lower.is_some() && r.lower_inc)),
        "upper_inc" => Ok(Value::Bool(!r.is_empty && r.upper.is_some() && r.upper_inc)),
        "lower_inf" => Ok(Value::Bool(!r.is_empty && r.lower.is_none())),
        "upper_inf" => Ok(Value::Bool(!r.is_empty && r.upper.is_none())),
        _ => Err(DbError::NotImplemented { feature: format!("range function: {name}") }),
    }
}
```

### Tests to fill out (complete list)

```rust
// Parser/DDL
range_create_table_int4range
range_create_table_all_five_types
range_insert_constructor_default_bounds    // int4range(1,10) → '[1,10)'
range_insert_constructor_explicit_bounds   // int4range(1,10,'[]') → '[1,11)' (canonicalized)
range_insert_cast_literal
range_insert_empty_literal
range_insert_unbounded_lower
range_insert_unbounded_upper

// Operators
range_contains_element_true
range_contains_element_false_exclusive_upper
range_contains_range_true
range_contains_range_false
range_contained_by_element
range_overlap_true
range_overlap_false_disjoint
range_union_overlapping
range_union_adjacent
range_union_disjoint_errors
range_intersection_overlapping
range_intersection_disjoint_empty
range_difference_non_overlapping
range_difference_interior_errors

// Scalar functions
range_lower_bounded
range_upper_bounded
range_lower_null_when_unbounded
range_isempty_true
range_isempty_false
range_lower_inc_true
range_upper_inf_true

// Comparison
range_equality
range_less_than

// NULL propagation
range_null_operand_returns_null
```

### Wire assertions (add to tools/wire-test.py)

```python
# ── Phase 20.13 — Range types ─────────────────────────────────────────────────
cur.execute("CREATE TABLE IF NOT EXISTS _wire_range (id INT, r INT4RANGE)")
cur.execute("DELETE FROM _wire_range")
cur.execute("INSERT INTO _wire_range VALUES (1, int4range(1, 10))")
cur.execute("INSERT INTO _wire_range VALUES (2, int4range(5, 15))")
conn.commit()

cur.execute("SELECT COUNT(*) FROM _wire_range WHERE r @> 5")
ok("[20.13a range_contains_elem] int4range @> 5 returns matching row",
   cur.fetchone()[0] == 2)

cur.execute("SELECT COUNT(*) FROM _wire_range WHERE r && int4range(8, 20)")
ok("[20.13b range_overlap] int4range && overlap returns 2 rows",
   cur.fetchone()[0] == 2)

cur.execute("SELECT isempty('empty'::int4range)")
ok("[20.13c range_isempty] isempty('empty'::int4range) returns true",
   cur.fetchone()[0] == 1)
```

### Verification against spec

- [ ] `Value::Range`, `DataType::Range`, `ColumnType::Range = 14` exist
- [ ] Codec round-trip for all 5 range types (bounded, unbounded, empty)
- [ ] `CREATE TABLE t (r INT4RANGE)` works
- [ ] `INSERT INTO t VALUES (int4range(1, 10))` works
- [ ] `'[1,10)'::int4range` cast works
- [ ] `r @> 5` / `r @> r2` containment correct
- [ ] `r1 && r2` overlap correct
- [ ] `r1 + r2`, `r1 * r2`, `r1 - r2` work
- [ ] `lower()`, `upper()`, `isempty()`, `lower_inc()`, `upper_inc()`, `lower_inf()`, `upper_inf()` work
- [ ] `=`, `<>`, `<`, `>`, `<=`, `>=` comparisons work
- [ ] All 5 type names work
- [ ] Integer canonicalization: `[1,5]` → `[1,6)`
- [ ] `daterange` canonicalization applies
- [ ] Disjoint union / interior difference return `DbError::InvalidValue`
- [ ] `cargo nextest run --workspace` passes
- [ ] `cargo clippy --workspace -- -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] Wire assertions pass

### Final commit

```
feat(fase-20): complete subphase 20.13 — range types

Implements specs/fase-20/spec-range-types.md
Plan: specs/fase-20/plan-range-types.md
Tests: ~25 integration tests, 3 wire assertions
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|---|---|---|
| `lower`/`upper` name collision with string functions | certain | priority match in mod.rs evaluates arg, dispatches by type |
| `Value` match arm cascade across crates | medium | grep all crates in Step 1 before coding |
| Discrete canonicalization logic subtle for edge cases | medium | unit tests for all boundary cases in Step 1 |
| `eval_binary_range` dispatched before NULL check breaks NULL propagation | low | keep NULL check at top of `eval_binary_range` |
| `+` for ranges conflicts with text concatenation dispatch | low | Range arm is checked before arithmetic arm |

## Rollback plan

1. `git reset --hard <commit before Step 1>`
2. Update spec status back to `approved` with note

## Estimated effort

Total: ~4h
Per step: step 1: 90min, step 2: 60min, step 3: 90min
