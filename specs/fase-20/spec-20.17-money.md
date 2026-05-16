# Spec: 20.17 MONEY type with multi-currency arithmetic

Phase: 20 — Types + import/export
Task: 20.17 — MONEY scalar type, exchange rate catalog, arithmetic and conversion
Status: implemented

## Context

Phase 20 extends AxiomDB's type system. Subphase 20.16 added the holiday calendar
catalog pattern (catalog heap + session cache + DDL fence) which is directly reused
here for exchange rates. This subphase adds a first-class `MONEY` column type that
stores an (amount, currency) pair and enforces currency homogeneity at the SQL layer,
pushing exchange-rate logic into the database instead of application code.

## Goal

Add a `MONEY` column type, an exchange rate catalog backed by a new catalog heap, and
scalar functions `CONVERT`, `CURRENCY_OF`, and `AMOUNT_OF` with exact-arithmetic
same-currency `+`/`-` operators.

## Non-goals

- Historical exchange rates (`AS OF date`) — deferred; only one rate per ordered pair
  at a time is stored.
- Automatic cross-currency arithmetic — deliberately rejected; mixing currencies without
  `CONVERT` is always an error.
- `MONEY * MONEY` or `MONEY / MONEY` — nonsensical; only `MONEY ± MONEY` (same currency)
  and `MONEY * numeric` / `MONEY / numeric` are supported.
- `MONEY(precision, scale)` parametrized column type — scale is implicit in the value.
- `MONEY` indexes or B-Tree ordering across currencies.

## Behavior

### Value representation

```rust
// axiomdb-types/src/value.rs — new variant after Range
/// MONEY = exact decimal amount + ISO 4217 3-byte currency code (e.g. b"USD").
/// Mantissa and scale follow the same convention as Decimal(i128, u8):
///   amount = mantissa × 10^(-scale)
Value::Money(i128, u8, [u8; 3])
```

Currency codes are stored as raw ASCII bytes, always upper-cased.
`[b'U', b'S', b'D']` is the canonical form; spaces pad to 3 bytes if the code
is shorter (codes shorter than 3 chars are allowed, e.g. `b"X \0"` — but in practice
ISO 4217 codes are always 3 chars).

### Column type

```rust
// axiomdb-catalog/src/schema_database.rs
ColumnType::Money = 15
```

`ColumnType::Money` maps to `DataType::Money` in the SQL layer and to
`Value::Money(_, _, _)` at runtime.

### DDL — exchange rate catalog

```sql
-- Register (or replace) the rate from_currency → to_currency.
-- rate is a positive DECIMAL literal (or integer).
CREATE EXCHANGE RATE 'USD' TO 'EUR' 0.9200;

-- Drop a single directional rate.
DROP EXCHANGE RATE 'USD' TO 'EUR';

-- Idempotent drop.
DROP EXCHANGE RATE IF EXISTS 'USD' TO 'EUR';

-- Column type DDL
CREATE TABLE invoices (
    id    BIGINT PRIMARY KEY,
    total MONEY NOT NULL
);
```

Exchange rates are **directional**: `USD → EUR` and `EUR → USD` are stored separately
and may have different values. `CREATE EXCHANGE RATE` replaces any existing rate for
that ordered pair.

### DDL AST nodes

```rust
pub struct CreateExchangeRateStmt {
    pub from_currency: String,  // uppercased, max 3 bytes
    pub to_currency:   String,  // uppercased, max 3 bytes
    pub rate:          String,  // decimal literal string, parsed to (i128, u8)
}

pub struct DropExchangeRateStmt {
    pub if_exists:     bool,
    pub from_currency: String,
    pub to_currency:   String,
}
```

### Exchange rate catalog — binary layout

Stored in the `axiom_exchange_rates` catalog heap, meta offset **192**
(`CATALOG_EXCHANGE_RATES_ROOT_BODY_OFFSET`).

```
ExchangeRateDef binary:
  [from_len: u8]           — length of from_currency UTF-8 bytes (≤ 3)
  [from:     from_len bytes]
  [to_len:   u8]           — length of to_currency UTF-8 bytes (≤ 3)
  [to:       to_len bytes]
  [mantissa: 16 bytes i128 LE]
  [scale:    1 byte  u8]
```

Total: 2 + from_len + to_len + 17 bytes per row.

```rust
pub struct ExchangeRateDef {
    pub from_currency: String,  // uppercase, ≤ 3 bytes
    pub to_currency:   String,  // uppercase, ≤ 3 bytes
    pub mantissa:      i128,
    pub scale:         u8,      // rate = mantissa × 10^(-scale)
}
```

Key for upsert/delete: `(from_currency, to_currency)` pair (both uppercased).

### Catalog writer API

```rust
pub fn upsert_exchange_rate(
    &mut self,
    def: &ExchangeRateDef,
) -> Result<(), DbError>;

pub fn delete_exchange_rate(
    &mut self,
    from: &str,
    to: &str,
    if_exists: bool,
) -> Result<(), DbError>;

// InvalidValue if not found and if_exists = false
```

### Catalog reader API

```rust
pub fn get_exchange_rate(
    &mut self,
    from: &str,
    to: &str,
) -> Result<Option<ExchangeRateDef>, DbError>;

pub fn list_exchange_rates(&mut self) -> Result<Vec<ExchangeRateDef>, DbError>;
```

### Session cache

```rust
// SessionContext field — cleared by invalidate_all()
pub exchange_rate_cache: HashMap<(String, String), (i128, u8)>
```

Key: `(from_uppercase, to_uppercase)`. Cleared on `invalidate_all()` (same DDL fence
as `holiday_cache`).

### Scalar functions

#### MONEY constructor

```sql
MONEY(amount, currency_code)
-- amount: DECIMAL or INT or BIGINT or REAL literal/expression
-- currency_code: TEXT, exactly 1–3 ASCII letters
-- returns: Value::Money(mantissa, scale, [u8; 3])
```

#### CONVERT

```sql
CONVERT(money_value, target_currency)
-- money_value:     Value::Money
-- target_currency: TEXT (3-char currency code)
-- returns:         Value::Money with the converted amount in target_currency
-- error:           InvalidValue if no exchange rate registered for (from, to)
-- same-currency:   returns input unchanged (no rate lookup needed)
```

Conversion arithmetic (exact, no float):
```
result_mantissa = money_mantissa × rate_mantissa
result_scale    = money_scale + rate_scale
```
Trailing zeros may optionally be stripped from `result_scale` to avoid unbounded growth,
but correctness takes priority over compactness.

#### CURRENCY_OF

```sql
CURRENCY_OF(money_value)
-- returns: Value::Text — the 3-char (or shorter) currency code in uppercase
```

#### AMOUNT_OF

```sql
AMOUNT_OF(money_value)
-- returns: Value::Decimal(mantissa, scale) — the numeric amount without currency
```

### Arithmetic operators

| Expression | Rule |
|-----------|------|
| `MONEY + MONEY` | Same currency → `Value::Money(m1+m2_normalized, scale, currency)` |
| `MONEY - MONEY` | Same currency → `Value::Money(m1-m2_normalized, scale, currency)` |
| `MONEY * INT\|BIGINT\|DECIMAL` | Scales the amount: `Value::Money(m*factor, scale, currency)` |
| `MONEY / INT\|BIGINT\|DECIMAL` | Integer division of mantissa; no float involved |
| `MONEY + MONEY` (different currencies) | `DbError::InvalidValue` |
| `MONEY + DECIMAL\|INT` | `DbError::InvalidValue` — currency is ambiguous |

When adding/subtracting two MONEY values with the same currency but different scales,
normalize both to the larger scale before operating:
```
MONEY(100, 2, USD) + MONEY(5000, 3, USD)
= MONEY(1000, 3, USD) + MONEY(5000, 3, USD)
= MONEY(6000, 3, USD) = 6.000 USD
```

### Wire display

MONEY values are serialized to the MySQL wire as `Text` in the format:

```
"<amount_decimal> <currency_code>"
-- e.g. "100.50 USD", "0.00 EUR", "1234 JPY"
```

Amount is formatted as a decimal string: mantissa formatted with the decimal point
inserted `scale` digits from the right. If `scale = 0`, no decimal point.
Examples: `Decimal(100_50, 2)` → `"100.50"`, `Decimal(1234, 0)` → `"1234"`.

### Error cases

| Input | Expected error | Condition |
|-------|----------------|-----------|
| `MONEY('abc', 'USD')` | `DbError::InvalidValue` | first arg not numeric |
| `MONEY(1.0, 'TOOLONG')` | `DbError::InvalidValue` | currency > 3 bytes |
| `MONEY(1.0, '')` | `DbError::InvalidValue` | empty currency code |
| `CONVERT(m, 'EUR')` when no rate USD→EUR | `DbError::InvalidValue` | missing rate |
| `MONEY(100 USD) + MONEY(100 EUR)` | `DbError::InvalidValue` | currency mismatch |
| `DROP EXCHANGE RATE 'X' TO 'Y'` (not found) | `DbError::InvalidValue` | missing, no IF EXISTS |
| `CREATE EXCHANGE RATE 'USD' TO 'USD' 1.0` | allowed | trivial identity rate |
| `CREATE EXCHANGE RATE 'USD' TO 'EUR' -0.5` | `DbError::InvalidValue` | negative rate |
| `CREATE EXCHANGE RATE 'USD' TO 'EUR' 0` | `DbError::InvalidValue` | zero rate |

## Edge cases

- [ ] `MONEY(0, 'USD')` — zero amount, valid
- [ ] `MONEY(-100.50, 'EUR')` — negative amount, valid (debits)
- [ ] `CONVERT(money, same_currency)` — no rate lookup, returns input unchanged
- [ ] NULL propagation — `MONEY(NULL, 'USD')` and `CONVERT(NULL, 'EUR')` return NULL
- [ ] `AMOUNT_OF(NULL)` and `CURRENCY_OF(NULL)` return NULL
- [ ] Exchange rate with scale 0 (e.g. `CREATE EXCHANGE RATE 'JPY' TO 'KRW' 9`)
- [ ] Very large mantissa — stays within i128 bounds; overflow → `InvalidValue`
- [ ] Currency code case normalization — `'usd'` → stored as `'USD'`
- [ ] `ColumnType::Money = 15` — update test that asserts `try_from(15).is_err()`

## On-disk format

### ColumnDef with Money column

`col_type = 15` (Money). No extra trailing bytes needed (Money has no sub-type
parameter like Array does). The stored value in heap rows uses the codec in
`axiomdb-types/src/codec.rs` — new codec arm for `Value::Money`.

### Heap row codec for Money values

```
[tag: 1 byte = 0x10]   — new tag in the row codec (next after Range tag)
[from_currency_len: 1 byte]... wait — not from/to, it's the stored value:
[mantissa: 16 bytes i128 LE]
[scale:    1 byte u8]
[currency: 3 bytes ASCII, space-padded on the right if < 3 chars]
```
Total: 20 bytes per MONEY cell in a heap row (after the 1-byte tag = 21 bytes).

## Performance budget

| Operation | Target |
|-----------|--------|
| `IS_BUSINESS_DAY` / `CONVERT` with hot cache | < 1 µs |
| `CONVERT` on cache miss (one catalog scan) | < 100 µs |

## Dependencies

- Depends on: spec-20.16 implemented (session cache + invalidate_all pattern)
- Blocks: none

## Open questions

All resolved:

- **Historical rates (`AS OF date`)?** → Deferred. Single current rate per pair.
- **Implicit cross-currency conversion?** → No. Always explicit `CONVERT`.
- **`MONEY * MONEY`?** → Error. Nonsensical semantically.
- **Column type parameter `MONEY(USD)`?** → No. Currency is in the value, not the type.

## Done criteria

- [ ] `Value::Money(i128, u8, [u8; 3])` exists in `axiomdb-types`
- [ ] `ColumnType::Money = 15` in `axiomdb-catalog`; `try_from(15)` returns `Ok`
- [ ] `ExchangeRateDef` with `to_bytes`/`from_bytes` roundtrip tested
- [ ] `upsert_exchange_rate` / `delete_exchange_rate` / `get_exchange_rate` implemented
- [ ] `CREATE EXCHANGE RATE` and `DROP EXCHANGE RATE [IF EXISTS]` parse correctly
- [ ] `CREATE TABLE t (col MONEY)` DDL works end-to-end
- [ ] `MONEY(amount, currency)` constructor works in SELECT
- [ ] `CONVERT(money, target)` uses catalog rates with session cache
- [ ] Same-currency `+` and `-` produce correct results
- [ ] Cross-currency `+`/`-` returns `InvalidValue`
- [ ] `CURRENCY_OF` and `AMOUNT_OF` return correct types
- [ ] NULL propagation for all functions
- [ ] Negative rate → `InvalidValue`; zero rate → `InvalidValue`
- [ ] `cargo nextest run -p axiomdb-catalog` passes
- [ ] `cargo nextest run -p axiomdb-sql --test integration_money` passes (≥ 20 tests)
- [ ] `cargo clippy --workspace -- -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] 4 wire assertions added to `tools/wire-test.py`

## References

- `specs/fase-20/spec-20.16-business-calendar.md` — exchange rate catalog follows same pattern
- `crates/axiomdb-catalog/src/schema_holiday_calendar.rs` — binary layout reference
- `crates/axiomdb-catalog/src/bootstrap.rs` — lazy-init heap root pattern
- ISO 4217 currency codes: https://www.iso.org/iso-4217-currency-codes.html
- PostgreSQL `money` type: https://www.postgresql.org/docs/current/datatype-money.html
  (AxiomDB's MONEY is more flexible — stores currency per-value, not per-database)
