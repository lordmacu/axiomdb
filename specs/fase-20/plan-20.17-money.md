# Plan: 20.17 MONEY type with multi-currency arithmetic

Phase: 20 — Types + import/export
Task: 20.17 — MONEY scalar type, exchange rate catalog, arithmetic and conversion
Spec: specs/fase-20/spec-20.17-money.md
Status: in-progress

## Summary

Five steps, each producing a clean commit. Step 1 adds `Value::Money` to axiomdb-types
and `ColumnType::Money = 15` to axiomdb-catalog with codec and coercion support. Step 2
builds the `axiom_exchange_rates` catalog heap (meta offset 192) following the 20.16
holiday-calendar pattern exactly. Step 3 adds the AST nodes and parser for
`CREATE/DROP EXCHANGE RATE` DDL and for `MONEY(amount, currency)` as a constructor
function and `MONEY` as a column type. Step 4 wires up the DDL executor, session cache,
and scalar functions (`CONVERT`, `CURRENCY_OF`, `AMOUNT_OF`) plus Money binary
arithmetic in `eval/ops.rs`. Step 5 adds ≥ 20 integration tests and 4 wire assertions.

## Dependencies

Must be done first:
- [x] spec-20.17-money.md approved

Blocks:
- [ ] 20.18 composite types

## Affected files

New files:
- `crates/axiomdb-catalog/src/schema_exchange_rate.rs` — `ExchangeRateDef` binary layout
- `crates/axiomdb-sql/src/executor/ddl_exchange_rate.rs` — DDL executor functions
- `crates/axiomdb-sql/src/executor/money_runtime.rs` — scalar functions + arithmetic
- `crates/axiomdb-sql/tests/integration_money.rs` — integration test suite

Modified files:
- `crates/axiomdb-types/src/value.rs` — add `Value::Money(i128, u8, [u8; 3])`
- `crates/axiomdb-types/src/types.rs` — add `DataType::Money`
- `crates/axiomdb-types/src/codec.rs` — encode/decode for Money
- `crates/axiomdb-types/src/coerce_api.rs` — coerce Money in type checks
- `crates/axiomdb-catalog/src/schema_database.rs` — `ColumnType::Money = 15`
- `crates/axiomdb-catalog/src/schema.rs` — update roundtrip test (Money variant + fix `try_from(15).is_err()`)
- `crates/axiomdb-catalog/src/bootstrap.rs` — `ensure_exchange_rates_root()`
- `crates/axiomdb-catalog/src/reader.rs` — `get_exchange_rate`, `list_exchange_rates`
- `crates/axiomdb-catalog/src/writer.rs` — `upsert_exchange_rate`, `delete_exchange_rate`
- `crates/axiomdb-catalog/src/lib.rs` — re-exports
- `crates/axiomdb-storage/src/meta.rs` — `CATALOG_EXCHANGE_RATES_ROOT_BODY_OFFSET = 192`
- `crates/axiomdb-storage/src/lib.rs` — re-export the new constant
- `crates/axiomdb-sql/src/ast.rs` — `CreateExchangeRateStmt`, `DropExchangeRateStmt`
- `crates/axiomdb-sql/src/parser/ddl.rs` — parse DDL
- `crates/axiomdb-sql/src/parser/mod.rs` — dispatch `EXCHANGE RATE`
- `crates/axiomdb-sql/src/parser/expr.rs` (or equivalent) — `MONEY(a, c)` constructor, `DataType::Money`
- `crates/axiomdb-sql/src/plan_deps.rs` — new stmt variants → `Ok(())`
- `crates/axiomdb-sql/src/executor/exec_explain.rs` — stub for new stmts
- `crates/axiomdb-sql/src/executor/exec_dispatch.rs` — wire DDL + `invalidate_all()`
- `crates/axiomdb-sql/src/executor/exec_subquery.rs` — chain `eval_money_function`
- `crates/axiomdb-sql/src/executor/mod.rs` — include new files
- `crates/axiomdb-sql/src/session.rs` — `exchange_rate_cache: HashMap<(String, String), (i128, u8)>`
- `crates/axiomdb-sql/src/eval/ops.rs` — Money binary arithmetic dispatch
- `tools/wire-test.py` — 4 new wire assertions

---

## Step 1 — Value::Money + ColumnType::Money + codec

**Goal:** Add the `Money` variant to the value and type systems; teach the codec to
encode/decode it; update coercion; fix the ColumnType discriminant test.
**Files:** `axiomdb-types/src/value.rs`, `axiomdb-types/src/types.rs`,
`axiomdb-types/src/codec.rs`, `axiomdb-types/src/coerce_api.rs`,
`axiomdb-catalog/src/schema_database.rs`, `axiomdb-catalog/src/schema.rs`

### Tests to add

```rust
// crates/axiomdb-catalog/src/schema.rs (existing test mod)
#[test]
fn test_column_type_money_roundtrip() {
    let byte: u8 = ColumnType::Money.into();
    assert_eq!(byte, 15);
    let back = ColumnType::try_from(15u8).unwrap();
    assert_eq!(back, ColumnType::Money);
}

// Update existing test: try_from(15) used to be Err, now is Ok
// Change: assert!(ColumnType::try_from(15).is_err())  →  assert!(ColumnType::try_from(16).is_err())

// crates/axiomdb-types/src/value.rs (inline mod tests)
#[test]
fn money_display() {
    assert_eq!(Value::Money(10050, 2, *b"USD").to_string(), "100.50 USD");
    assert_eq!(Value::Money(1234, 0, *b"JPY").to_string(), "1234 JPY");
    assert_eq!(Value::Money(-500, 2, *b"EUR").to_string(), "-5.00 EUR");
    assert_eq!(Value::Money(0, 0, *b"USD").to_string(), "0 USD");
}

// crates/axiomdb-types/src/codec.rs (inline mod tests)
#[test]
fn money_codec_roundtrip() {
    let schema = vec![DataType::Money];
    let row    = vec![Value::Money(10050_i128, 2_u8, *b"USD")];
    let bytes  = encode_row(&row, &schema).unwrap();
    let back   = decode_row(&bytes, &schema).unwrap();
    assert_eq!(back, row);
}

#[test]
fn money_null_codec_roundtrip() {
    let schema = vec![DataType::Money];
    let row    = vec![Value::Null];
    let bytes  = encode_row(&row, &schema).unwrap();
    let back   = decode_row(&bytes, &schema).unwrap();
    assert_eq!(back[0], Value::Null);
}
```

### Implementation outline

```rust
// crates/axiomdb-types/src/value.rs — add after Range variant
/// MONEY = exact decimal amount + ISO 4217 currency code.
/// money_amount = mantissa × 10^(-scale)
Money(i128, u8, [u8; 3]),

// Display impl:
Value::Money(m, s, c) => {
    let currency = std::str::from_utf8(c).unwrap_or("???").trim_end_matches('\0');
    if *s == 0 {
        write!(f, "{m} {currency}")
    } else {
        // Format mantissa with decimal point inserted s digits from the right
        let abs_m = m.unsigned_abs();
        let divisor = 10u128.pow(*s as u32);
        let int_part = abs_m / divisor;
        let frac_part = abs_m % divisor;
        let sign = if *m < 0 { "-" } else { "" };
        write!(f, "{sign}{int_part}.{frac_part:0>width$} {currency}", width = *s as usize)
    }
}

// crates/axiomdb-types/src/types.rs — add after Range
Money,

// crates/axiomdb-catalog/src/schema_database.rs
Money = 15,

// TryFrom<u8>: add arm  15 => Ok(Self::Money)
// From<ColumnType> for u8: add arm  ColumnType::Money => 15

// crates/axiomdb-types/src/codec.rs
// encode_row: Money arm after Range
Value::Money(m, s, c) => {
    buf.extend_from_slice(&m.to_le_bytes());   // 16 bytes
    buf.push(*s);                               // 1 byte
    buf.extend_from_slice(c);                   // 3 bytes — total 20 bytes
}

// decode_row: DataType::Money arm
DataType::Money => {
    let mut mb = [0u8; 16];
    mb.copy_from_slice(&bytes[pos..pos+16]); pos += 16;
    let m = i128::from_le_bytes(mb);
    let s = bytes[pos]; pos += 1;
    let mut c = [0u8; 3];
    c.copy_from_slice(&bytes[pos..pos+3]); pos += 3;
    Value::Money(m, s, c)
}

// encoded_len: Money → 20 bytes (no length prefix, fixed size)

// crates/axiomdb-types/src/coerce_api.rs
// type_check_value: add arm (Value::Money(..), DataType::Money) => Ok(...)
```

### Verification

```bash
./tools/vm.sh test axiomdb-catalog
./tools/vm.sh test axiomdb-types
./tools/vm.sh clippy
```

### Commit

```
feat(fase-20): 20.17 step 1 — Value::Money + ColumnType::Money=15 + codec
```

---

## Step 2 — Exchange rate catalog heap

**Goal:** Persistent `axiom_exchange_rates` catalog heap: binary layout, bootstrap,
reader, writer. Following the holiday-calendar pattern from 20.16 exactly.
**Files:** `axiomdb-storage/src/meta.rs`, `axiomdb-storage/src/lib.rs`,
`axiomdb-catalog/src/schema_exchange_rate.rs` (NEW),
`axiomdb-catalog/src/bootstrap.rs`, `axiomdb-catalog/src/reader.rs`,
`axiomdb-catalog/src/writer.rs`, `axiomdb-catalog/src/lib.rs`

### Tests to add

```rust
// crates/axiomdb-catalog/src/schema_exchange_rate.rs (inline tests)
#[test]
fn roundtrip_exchange_rate() {
    let def = ExchangeRateDef {
        from_currency: "USD".into(),
        to_currency:   "EUR".into(),
        mantissa:      92,
        scale:         2,   // rate = 0.92
    };
    let bytes = def.to_bytes();
    let (back, consumed) = ExchangeRateDef::from_bytes(&bytes).unwrap();
    assert_eq!(back, def);
    assert_eq!(consumed, bytes.len());
}

#[test]
fn roundtrip_truncated_returns_error() {
    let def = ExchangeRateDef { from_currency: "USD".into(), to_currency: "EUR".into(),
                                mantissa: 92, scale: 2 };
    let bytes = def.to_bytes();
    assert!(ExchangeRateDef::from_bytes(&bytes[..bytes.len()-1]).is_err());
}

// crates/axiomdb-catalog/src/schema.rs (test module)
#[test]
fn upsert_and_get_exchange_rate() {
    let (mut storage, snap) = test_storage();   // helper already used in catalog tests
    let mut writer = CatalogWriter::new(&mut storage, snap).unwrap();
    let def = ExchangeRateDef { from_currency: "USD".into(), to_currency: "EUR".into(),
                                mantissa: 92, scale: 2 };
    writer.upsert_exchange_rate(&def).unwrap();
    let snap2 = writer.commit().unwrap();
    let mut reader = CatalogReader::new(&storage, snap2).unwrap();
    let got = reader.get_exchange_rate("USD", "EUR").unwrap().unwrap();
    assert_eq!(got, def);
}

#[test]
fn delete_exchange_rate_if_exists_idempotent() {
    let (mut storage, snap) = test_storage();
    let mut writer = CatalogWriter::new(&mut storage, snap).unwrap();
    writer.delete_exchange_rate("USD", "EUR", true).unwrap(); // no error
    writer.commit().unwrap();
}
```

### Implementation outline

```rust
// crates/axiomdb-storage/src/meta.rs
pub const CATALOG_EXCHANGE_RATES_ROOT_BODY_OFFSET: usize = 192;

// crates/axiomdb-catalog/src/schema_exchange_rate.rs
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExchangeRateDef {
    pub from_currency: String,  // uppercase, ≤ 3 bytes
    pub to_currency:   String,  // uppercase, ≤ 3 bytes
    pub mantissa:      i128,
    pub scale:         u8,
}

impl ExchangeRateDef {
    pub fn to_bytes(&self) -> Vec<u8> { ... }
    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, usize), DbError> { ... }
}

// crates/axiomdb-catalog/src/bootstrap.rs
// Add field: exchange_rates: u64
// Add in init(): exchange_rates: 0
// Add in page_ids(): read offset CATALOG_EXCHANGE_RATES_ROOT_BODY_OFFSET
// Add: pub fn ensure_exchange_rates_root(storage) -> Result<u64, DbError>

// crates/axiomdb-catalog/src/writer.rs
pub const SYSTEM_TABLE_EXCHANGE_RATES: u32 = u32::MAX - 16;

pub fn upsert_exchange_rate(&mut self, def: &ExchangeRateDef) -> Result<(), DbError> {
    // 1. ensure root exists
    // 2. scan heap — delete existing row where from==def.from && to==def.to
    // 3. insert new row
}

pub fn delete_exchange_rate(&mut self, from: &str, to: &str, if_exists: bool)
    -> Result<(), DbError> {
    // scan, delete matching row; if not found and !if_exists → InvalidValue
}

// crates/axiomdb-catalog/src/reader.rs
pub fn get_exchange_rate(&mut self, from: &str, to: &str)
    -> Result<Option<ExchangeRateDef>, DbError> { ... }

pub fn list_exchange_rates(&mut self) -> Result<Vec<ExchangeRateDef>, DbError> { ... }
```

### Verification

```bash
./tools/vm.sh test axiomdb-catalog
./tools/vm.sh test axiomdb-storage
./tools/vm.sh clippy
```

### Commit

```
feat(fase-20): 20.17 step 2 — exchange rate catalog heap (meta offset 192)
```

---

## Step 3 — AST + Parser

**Goal:** Parse `CREATE EXCHANGE RATE`, `DROP EXCHANGE RATE`, `MONEY(a, c)` constructor,
and `MONEY` as a column type in `CREATE TABLE`.
**Files:** `axiomdb-sql/src/ast.rs`, `axiomdb-sql/src/parser/ddl.rs`,
`axiomdb-sql/src/parser/mod.rs`, `axiomdb-sql/src/plan_deps.rs`,
`axiomdb-sql/src/executor/exec_explain.rs`
plus whichever file handles column type and expression parsing (`DataType::Money`,
`MONEY(...)` call).

### Tests to add

```rust
// crates/axiomdb-sql/src/parser/ (inline parser unit tests or
//   a new integration_money_parse test block)
#[test]
fn parse_create_exchange_rate() {
    let stmt = parse_one("CREATE EXCHANGE RATE 'USD' TO 'EUR' 0.92").unwrap();
    match stmt {
        Stmt::CreateExchangeRate(s) => {
            assert_eq!(s.from_currency, "USD");
            assert_eq!(s.to_currency, "EUR");
            assert_eq!(s.rate, "0.92");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn parse_drop_exchange_rate_if_exists() {
    let stmt = parse_one("DROP EXCHANGE RATE IF EXISTS 'USD' TO 'EUR'").unwrap();
    match stmt {
        Stmt::DropExchangeRate(s) => {
            assert!(s.if_exists);
            assert_eq!(s.from_currency, "USD");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn parse_create_table_money_column() {
    let stmt = parse_one("CREATE TABLE t (price MONEY NOT NULL)").unwrap();
    // confirm column has DataType::Money
}
```

### Implementation outline

```rust
// crates/axiomdb-sql/src/ast.rs
pub struct CreateExchangeRateStmt {
    pub from_currency: String,
    pub to_currency:   String,
    pub rate:          String,  // raw decimal literal string
}
pub struct DropExchangeRateStmt {
    pub if_exists:     bool,
    pub from_currency: String,
    pub to_currency:   String,
}
// Add Stmt::CreateExchangeRate(CreateExchangeRateStmt) and Stmt::DropExchangeRate(...)
// (after DropHolidayCalendar variants)

// parser/ddl.rs
// parse_create_exchange_rate:
//   expect Token::Ident("EXCHANGE") + Token::Ident("RATE")
//   parse_currency_code  → from
//   expect Token::To
//   parse_currency_code  → to
//   parse rate as numeric literal string (Token::IntLit or Token::DecimalLit)
//   validate rate > 0

// parse_drop_exchange_rate:
//   [IF EXISTS]  parse_currency_code  TO  parse_currency_code

// parser/mod.rs CREATE dispatch:
//   Token::Ident(kw) if kw.eq_ignore_ascii_case("exchange") → peek "rate" → ...

// DataType::Money parsing (wherever other DataType keywords are):
//   Token::Ident(kw) if kw.eq_ignore_ascii_case("money") → DataType::Money

// MONEY(amount, currency) constructor:
//   parsed as a FunctionCall("MONEY", [amount_expr, currency_expr])
//   — handled in the executor (eval_money_function), not in the parser
//   No special parser treatment needed.

// plan_deps.rs:
//   Stmt::CreateExchangeRate(_) | Stmt::DropExchangeRate(_) => Ok(())

// exec_explain.rs dispatch():
//   Stmt::CreateExchangeRate(_) | Stmt::DropExchangeRate(_) =>
//       Err(NotImplemented { ... })
```

**Note on `TO` keyword:** `TO` is already a lexer keyword (`Token::To`) used by
`CREATE TABLE ... AS`, so match `Token::To` directly (not `Token::Ident("to")`).

**Note on rate literal:** Accept both integer and decimal literals. Store as a String;
the executor will parse it to `(i128, u8)` at execution time.

### Verification

```bash
./tools/vm.sh test axiomdb-sql
./tools/vm.sh clippy
```

### Commit

```
feat(fase-20): 20.17 step 3 — AST + parser for CREATE/DROP EXCHANGE RATE + MONEY type
```

---

## Step 4 — DDL executor + session cache + scalar functions + arithmetic

**Goal:** Execute `CREATE/DROP EXCHANGE RATE`; add `exchange_rate_cache` to session;
implement `CONVERT`, `CURRENCY_OF`, `AMOUNT_OF`, `MONEY()` constructor; add Money
arithmetic dispatch in `eval_binary`.
**Files:** `axiomdb-sql/src/session.rs`,
`axiomdb-sql/src/executor/ddl_exchange_rate.rs` (NEW),
`axiomdb-sql/src/executor/money_runtime.rs` (NEW),
`axiomdb-sql/src/executor/exec_dispatch.rs`,
`axiomdb-sql/src/executor/exec_subquery.rs`,
`axiomdb-sql/src/executor/mod.rs`,
`axiomdb-sql/src/eval/ops.rs`

### Tests to add

(These will be superseded by full integration tests in Step 5, but add quick unit-level
tests for the arithmetic helpers.)

```rust
// in money_runtime.rs inline tests
#[test]
fn normalize_scales_same_currency() {
    // MONEY(100_00, 2, USD) + MONEY(5_000, 3, USD) = MONEY(6_000, 3, USD)
    let l = Value::Money(10000, 2, *b"USD");
    let r = Value::Money(5000, 3, *b"USD");
    let result = money_add(l, r).unwrap();
    assert_eq!(result, Value::Money(15000, 3, *b"USD"));
    // 10000×10^-2 + 5000×10^-3 = 1.0000 + 0.5000 = 1.5000 = 15000×10^-3? Wait.
    // 10000 × 10^-2 = 100.00; normalize to scale 3: mantissa = 100000
    // 100000 + 5000 = 105000 × 10^-3 = 105.000 — adjust test values accordingly
}
```

### Implementation outline

```rust
// crates/axiomdb-sql/src/session.rs
pub exchange_rate_cache: HashMap<(String, String), (i128, u8)>,
// init: HashMap::new()
// invalidate_all: self.exchange_rate_cache.clear()

// crates/axiomdb-sql/src/executor/ddl_exchange_rate.rs
fn parse_rate_literal(s: &str) -> Result<(i128, u8), DbError> {
    // parse "0.92" → (92, 2); "1" → (1, 0); reject negative, zero
}

pub(crate) fn execute_create_exchange_rate(
    stmt: CreateExchangeRateStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    conn_txn: &mut ConnTxn,
) -> Result<QueryResult, DbError> {
    let (mantissa, scale) = parse_rate_literal(&stmt.rate)?;
    // validate mantissa > 0
    let def = ExchangeRateDef { from_currency: stmt.from_currency.to_uppercase(),
                                to_currency: stmt.to_currency.to_uppercase(),
                                mantissa, scale };
    let mut writer = CatalogWriter::new(storage, conn_txn.snap)?;
    writer.upsert_exchange_rate(&def)?;
    conn_txn.snap = writer.commit()?;
    Ok(QueryResult::ok_rows_affected(0))
}

pub(crate) fn execute_drop_exchange_rate(...) -> Result<QueryResult, DbError> { ... }

// crates/axiomdb-sql/src/executor/money_runtime.rs
pub(crate) fn eval_money_function(
    name: &str, args: &[Expr], row: &[Value], runner: &mut ExecSubqueryRunner,
) -> Result<Option<Value>, DbError> {
    match name.to_uppercase().as_str() {
        "MONEY"       => Some(eval_money_constructor(args, row, runner)),
        "CONVERT"     => Some(eval_convert(args, row, runner)),
        "CURRENCY_OF" => Some(eval_currency_of(args, row, runner)),
        "AMOUNT_OF"   => Some(eval_amount_of(args, row, runner)),
        _             => None,
    }.transpose()
}

fn get_or_load_rate(
    from: &str, to: &str, runner: &mut ExecSubqueryRunner,
) -> Result<(i128, u8), DbError> {
    let key = (from.to_uppercase(), to.to_uppercase());
    if let Some(&rate) = runner.ctx.exchange_rate_cache.get(&key) {
        return Ok(rate);
    }
    let snap = runner.ctx.conn_txn.as_ref().map(|t| t.snap).unwrap_or_else(|| runner.txn.snapshot());
    let mut reader = CatalogReader::new(runner.storage, snap)?;
    let def = reader.get_exchange_rate(&key.0, &key.1)?
        .ok_or_else(|| DbError::InvalidValue { message: format!("no exchange rate {}->{}", key.0, key.1) })?;
    let rate = (def.mantissa, def.scale);
    runner.ctx.exchange_rate_cache.insert(key, rate);
    Ok(rate)
}

// crates/axiomdb-sql/src/eval/ops.rs
// In eval_binary, add before NULL propagation:
if matches!(&l, Value::Money(_,_,_)) || matches!(&r, Value::Money(_,_,_)) {
    return eval_binary_money(op, l, r);
}

fn eval_binary_money(op: BinaryOp, l: Value, r: Value) -> Result<Value, DbError> {
    match (op, &l, &r) {
        (BinaryOp::Add | BinaryOp::Sub, Value::Money(lm,ls,lc), Value::Money(rm,rs,rc)) => {
            if lc != rc {
                return Err(DbError::InvalidValue { message: "currency mismatch in Money arithmetic".into() });
            }
            // normalize to max scale
            ...
        }
        (BinaryOp::Mul, Value::Money(m,s,c), Value::Int(f))   => Ok(Value::Money(m*(*f as i128), *s, *c)),
        (BinaryOp::Mul, Value::Money(m,s,c), Value::BigInt(f)) => Ok(Value::Money(m*f, *s, *c)),
        (BinaryOp::Div, Value::Money(m,s,c), Value::Int(f))   => Ok(Value::Money(m/(*f as i128), *s, *c)),
        _ => Err(DbError::InvalidValue { message: format!("unsupported Money operator {op:?}") }),
    }
}
```

**Key: fully-qualified types in money_runtime.rs** (same pattern as business_calendar_runtime.rs —
use `std::collections::HashMap` etc. when not imported in mod.rs).

### Verification

```bash
./tools/vm.sh test axiomdb-sql
./tools/vm.sh clippy
```

### Commit

```
feat(fase-20): 20.17 step 4 — DDL executor + session cache + CONVERT / CURRENCY_OF / AMOUNT_OF
```

---

## Step 5 — Integration tests + wire smoke

**Goal:** ≥ 20 integration tests covering all spec edge cases; 4 wire assertions.
**Files:** `crates/axiomdb-sql/tests/integration_money.rs` (NEW),
`tools/wire-test.py`

### Test plan

```rust
// CREATE EXCHANGE RATE
create_exchange_rate_persists_catalog_entry
create_exchange_rate_replaces_existing
create_exchange_rate_normalizes_currency_to_uppercase
create_exchange_rate_zero_rate_returns_error
create_exchange_rate_negative_rate_returns_error
drop_exchange_rate_removes_entry
drop_exchange_rate_if_exists_idempotent
drop_exchange_rate_without_if_exists_errors_when_missing

// MONEY constructor
money_constructor_integer_amount
money_constructor_decimal_amount
money_constructor_negative_amount
money_constructor_null_amount_returns_null
money_constructor_empty_currency_returns_error
money_constructor_currency_too_long_returns_error

// CONVERT
convert_same_currency_returns_unchanged
convert_usd_to_eur_uses_catalog_rate
convert_cache_invalidated_after_create
convert_missing_rate_returns_error
convert_null_returns_null

// CURRENCY_OF / AMOUNT_OF
currency_of_returns_currency_code
amount_of_returns_decimal
currency_of_null_returns_null

// Arithmetic
money_add_same_currency
money_sub_same_currency
money_add_different_currencies_returns_error
money_mul_by_integer

// CREATE TABLE with MONEY column
create_table_with_money_column_and_insert_select
```

### Wire assertions to add to tools/wire-test.py

```python
# [20.17a] CREATE TABLE with MONEY column and INSERT/SELECT roundtrip
# [20.17b] IS_SAME_CURRENCY check via CURRENCY_OF
# [20.17c] CONVERT using catalog rate
# [20.17d] Cross-currency addition returns error
```

### Verification

```bash
./tools/vm.sh test axiomdb-sql          # all tests pass
./tools/vm.sh test --workspace          # workspace clean
./tools/vm.sh clippy
./tools/vm.sh fmt --check
# wire test (Lima): rebuild server + python3 tools/wire-test.py
```

### Final commit

```
feat(fase-20): 20.17 step 5 — integration tests + wire smoke (MONEY type)

Implements specs/fase-20/spec-20.17-money.md
Plan: specs/fase-20/plan-20.17-money.md
Tests: 25+ new integration tests, 4 wire assertions
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| `TO` keyword collision with `Token::To` in parser | low | `Token::To` is already a keyword — match it directly |
| `ColumnType::try_from(15).is_err()` test is hardcoded | certain | update that assertion in step 1 |
| Money arithmetic scale overflow (i128 × 10^N) | medium | check before multiplying; return InvalidValue on overflow |
| `include!` scope — HashMap import collision | low | use fully-qualified `std::collections::HashMap` in money_runtime.rs |
| Wire display of negative MONEY | low | test `money_display` covers negative in step 1 |

## Rollback plan

If abandoned mid-way:
1. `git reset --hard <last clean commit>`
2. Branch: `abandoned/plan-20.17-money-<date>`
3. Spec status → `draft` with note

## Estimated effort

Total: ~4 hours
Per step: step 1: 45min, step 2: 45min, step 3: 45min, step 4: 75min, step 5: 30min
