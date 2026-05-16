# Plan: 20.16 Business Calendar Functions

Phase: 20 — Types + import/export  
Task: Business calendar DDL + scalar functions  
Spec: specs/fase-20/spec-20.16-business-calendar.md  
Status: in-progress

## Summary

Five steps, each producing a compilable commit. Step 1 adds the catalog data
structure and storage machinery. Step 2 wires the AST and parser. Step 3
implements DDL execution. Step 4 implements the three scalar functions plus the
session cache. Step 5 covers integration tests and wire smoke. The TDD order
means every function is testable in isolation before the full pipeline exists.

## Dependencies

Must be done first:
- [x] spec-20.16-business-calendar.md approved

Blocks:
- [ ] 20.17 (MONEY type) — independent; no block

## Affected files

New files:
- `crates/axiomdb-catalog/src/schema_holiday_calendar.rs` — `HolidayCalendarDef` struct + serde
- `crates/axiomdb-sql/src/executor/business_calendar_runtime.rs` — scalar function impl (included in mod.rs)
- `crates/axiomdb-sql/src/executor/ddl_holiday_calendar.rs` — DDL executor (included in mod.rs)
- `crates/axiomdb-sql/tests/integration_business_calendar.rs` — integration tests

Modified files:
- `crates/axiomdb-storage/src/meta.rs` — add `CATALOG_HOLIDAY_CALENDARS_ROOT_BODY_OFFSET = 184`
- `crates/axiomdb-storage/src/lib.rs` — re-export new constant
- `crates/axiomdb-catalog/src/bootstrap.rs` — `holiday_calendars: u64` field + `ensure_holiday_calendars_root()`
- `crates/axiomdb-catalog/src/writer.rs` — `SYSTEM_TABLE_HOLIDAY_CALENDARS` const + `upsert_holiday_calendar` + `delete_holiday_calendar`
- `crates/axiomdb-catalog/src/reader.rs` — `get_holiday_calendar` method
- `crates/axiomdb-catalog/src/lib.rs` — re-export `HolidayCalendarDef`, `SYSTEM_TABLE_HOLIDAY_CALENDARS`
- `crates/axiomdb-sql/src/ast.rs` — `Stmt::CreateHolidayCalendar`, `Stmt::DropHolidayCalendar`, two new structs
- `crates/axiomdb-sql/src/parser/mod.rs` — dispatch `CREATE HOLIDAY` and `DROP HOLIDAY`
- `crates/axiomdb-sql/src/parser/ddl.rs` — `parse_create_holiday_calendar`, `parse_drop_holiday_calendar`
- `crates/axiomdb-sql/src/executor/mod.rs` — `include!` the two new files
- `crates/axiomdb-sql/src/executor/exec_dispatch.rs` — match new Stmt variants
- `crates/axiomdb-sql/src/executor/exec_subquery.rs` — call `eval_business_calendar_function` in `eval_function`
- `crates/axiomdb-sql/src/session.rs` — add `holiday_cache: HashMap<String, std::sync::Arc<std::collections::HashSet<i32>>>`
- `tools/wire-test.py` — 4+ new assertions

---

## Step 1 — Catalog layer: HolidayCalendarDef + storage + bootstrap + reader/writer

**Goal:** Persist and retrieve `HolidayCalendarDef` via the standard catalog-heap machinery.

**Files:**
- `crates/axiomdb-storage/src/meta.rs` (1 constant)
- `crates/axiomdb-storage/src/lib.rs` (re-export)
- `crates/axiomdb-catalog/src/schema_holiday_calendar.rs` (new)
- `crates/axiomdb-catalog/src/bootstrap.rs` (1 field + 1 method)
- `crates/axiomdb-catalog/src/writer.rs` (1 const + 2 methods)
- `crates/axiomdb-catalog/src/reader.rs` (1 method)
- `crates/axiomdb-catalog/src/lib.rs` (re-exports)

### 1a — meta.rs: new constant

```rust
// crates/axiomdb-storage/src/meta.rs
/// body offset of `catalog_holiday_calendars_root: u64` — root heap page for
/// `axiom_holiday_calendars` (Phase 20.16). Value 0 = not yet allocated (lazily
/// initialized on first `CREATE HOLIDAY CALENDAR` statement).
pub const CATALOG_HOLIDAY_CALENDARS_ROOT_BODY_OFFSET: usize = 184;
```

Re-export in `crates/axiomdb-storage/src/lib.rs`.

### 1b — schema_holiday_calendar.rs

```rust
// crates/axiomdb-catalog/src/schema_holiday_calendar.rs
use axiomdb_core::error::DbError;

/// Persistent definition of a country holiday calendar (Phase 20.16).
///
/// Binary layout:
///   [code_len: u8][country_code: utf8 N bytes][count: u16 LE][holidays: i32 LE × count]
///
/// `country_code` is always stored upper-cased. `holidays` are days since
/// Unix epoch (1970-01-01 = 0), sorted ascending, deduplicated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HolidayCalendarDef {
    /// Upper-cased country code, max 16 bytes.
    pub country_code: String,
    /// Sorted, deduplicated holiday dates as days-since-epoch.
    pub holidays: Vec<i32>,
}

impl HolidayCalendarDef {
    pub fn to_bytes(&self) -> Vec<u8> { ... }
    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, usize), DbError> { ... }
}
```

**to_bytes layout:**
```
[code_len: u8]
[code_bytes: N]
[count: u16 LE]
[i32 LE × count]
```

**Validation in to_bytes:** `debug_assert!(code.len() <= 16)`.

### 1c — bootstrap.rs

Add field to `CatalogPageIds`:
```rust
/// Root page of the `axiom_holiday_calendars` heap (Phase 20.16).
/// Zero on legacy databases; lazily initialized on first `CREATE HOLIDAY CALENDAR`.
pub holiday_calendars: u64,
```

Update `ensure_database_roots` to read the field from the meta page.

Add method:
```rust
pub fn ensure_holiday_calendars_root(storage: &dyn StorageEngine) -> Result<u64, DbError> {
    let root = read_meta_u64(storage, CATALOG_HOLIDAY_CALENDARS_ROOT_BODY_OFFSET)?;
    if root != 0 { return Ok(root); }
    let new_root = storage.alloc_page(PageType::Data)?;
    let page = Page::new(PageType::Data, new_root);
    storage.write_page(new_root, &page)?;
    write_meta_u64(storage, CATALOG_HOLIDAY_CALENDARS_ROOT_BODY_OFFSET, new_root)?;
    storage.flush()?;
    Ok(new_root)
}
```

### 1d — writer.rs

```rust
pub const SYSTEM_TABLE_HOLIDAY_CALENDARS: u32 = u32::MAX - 15;

/// Inserts or replaces a holiday calendar in `axiom_holiday_calendars`.
/// If a calendar for the same country code already exists, deletes it first.
pub fn upsert_holiday_calendar(&mut self, def: HolidayCalendarDef) -> Result<(), DbError> {
    let root = CatalogBootstrap::ensure_holiday_calendars_root(self.storage)?;
    self.page_ids.holiday_calendars = root;
    // delete old entry if present (same pattern as upsert_cron_job)
    // insert new entry
}

/// Deletes a holiday calendar. Returns `true` if found and deleted.
pub fn delete_holiday_calendar(&mut self, country: &str) -> Result<bool, DbError> {
    // heap scan, case-insensitive match, delete if found
}
```

### 1e — reader.rs

```rust
/// Loads the holiday calendar for `country` (case-insensitive). Returns `None`
/// when the root is uninitialized or no entry exists for this country code.
pub fn get_holiday_calendar(
    &mut self,
    country: &str,
) -> Result<Option<HolidayCalendarDef>, DbError> {
    let root = self.page_ids.holiday_calendars;
    if root == 0 { return Ok(None); }
    let rows = HeapChain::scan_visible_ro(self.storage, root, self.snapshot.clone())?;
    for (_, _, data) in rows {
        let (def, _) = HolidayCalendarDef::from_bytes(&data)?;
        if def.country_code.eq_ignore_ascii_case(country) {
            return Ok(Some(def));
        }
    }
    Ok(None)
}
```

### Test to add (catalog unit test)

```rust
// crates/axiomdb-catalog/src/schema_holiday_calendar.rs (inline #[cfg(test)])
#[test]
fn roundtrip_empty_calendar() {
    let def = HolidayCalendarDef { country_code: "XX".into(), holidays: vec![] };
    let bytes = def.to_bytes();
    let (decoded, consumed) = HolidayCalendarDef::from_bytes(&bytes).unwrap();
    assert_eq!(decoded, def);
    assert_eq!(consumed, bytes.len());
}

#[test]
fn roundtrip_with_holidays() {
    let mut holidays = vec![20000i32, 20010, 20020, 20005]; // unsorted input
    let def = HolidayCalendarDef { country_code: "CO".into(), holidays: holidays.clone() };
    // to_bytes sorts — after sort: 20000, 20005, 20010, 20020
    let bytes = def.to_bytes();
    let (decoded, _) = HolidayCalendarDef::from_bytes(&bytes).unwrap();
    assert_eq!(decoded.country_code, "CO");
    assert_eq!(decoded.holidays, vec![20000, 20005, 20010, 20020]);
}

#[test]
fn roundtrip_case_normalized() {
    let def = HolidayCalendarDef { country_code: "us".into(), holidays: vec![19723] };
    let bytes = def.to_bytes();
    let (decoded, _) = HolidayCalendarDef::from_bytes(&bytes).unwrap();
    assert_eq!(decoded.country_code, "US"); // stored upper-cased
}
```

### Verification

```bash
./tools/vm.sh test -- -p axiomdb-catalog 2>&1 | tail -5
./tools/vm.sh clippy 2>&1 | tail -5
```

### Commit

```
feat(fase-20): 20.16 step 1 — HolidayCalendarDef catalog layer

- meta.rs: CATALOG_HOLIDAY_CALENDARS_ROOT_BODY_OFFSET = 184
- schema_holiday_calendar.rs: HolidayCalendarDef { country_code, holidays }
  binary serde, to_bytes sorts+deduplicates, from_bytes validates
- bootstrap.rs: holiday_calendars field + ensure_holiday_calendars_root()
- writer.rs: SYSTEM_TABLE_HOLIDAY_CALENDARS + upsert/delete methods
- reader.rs: get_holiday_calendar(country) with case-insensitive match
- 3 roundtrip unit tests

Step 1 of specs/fase-20/plan-20.16-business-calendar.md
```

---

## Step 2 — AST + parser

**Goal:** Parse `CREATE HOLIDAY CALENDAR 'XX' (dates...)` and `DROP HOLIDAY CALENDAR [IF EXISTS] 'XX'`  
**Files:** `ast.rs`, `parser/mod.rs`, `parser/ddl.rs`

### 2a — ast.rs: two new structs + two new Stmt variants

```rust
/// Phase 20.16 — `CREATE HOLIDAY CALENDAR 'code' ('date1', ...)`.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateHolidayCalendarStmt {
    /// Country code string literal (e.g. `'CO'`). Normalised to upper-case at exec time.
    pub country_code: String,
    /// Raw date string literals — validated and converted to i32 at DDL execution.
    pub date_literals: Vec<String>,
}

/// Phase 20.16 — `DROP HOLIDAY CALENDAR [IF EXISTS] 'code'`.
#[derive(Debug, Clone, PartialEq)]
pub struct DropHolidayCalendarStmt {
    pub country_code: String,
    pub if_exists: bool,
}
```

Add to `Stmt` enum:
```rust
CreateHolidayCalendar(CreateHolidayCalendarStmt),
DropHolidayCalendar(DropHolidayCalendarStmt),
```

### 2b — parser/mod.rs: dispatch

In `parse_create()`, before the final `other =>` fallback:
```rust
Token::Ident(kw) if kw.eq_ignore_ascii_case("holiday") => {
    self.advance(); // consume HOLIDAY
    self.expect_ident_ci("calendar")?; // consume CALENDAR
    ddl::parse_create_holiday_calendar(self)
}
```

In `parse_drop()`, before the final `other =>` fallback:
```rust
Token::Ident(kw) if kw.eq_ignore_ascii_case("holiday") => {
    self.advance(); // consume HOLIDAY
    self.expect_ident_ci("calendar")?; // consume CALENDAR
    ddl::parse_drop_holiday_calendar(self)
}
```

(`expect_ident_ci` is a helper that advances and errors if the next token is not
the given ident — check if it exists or add it; several alternatives exist in the
parser.)

### 2c — parser/ddl.rs: two parse functions

```rust
/// `CREATE HOLIDAY CALENDAR 'code' ('YYYY-MM-DD' [, ...])`
pub(crate) fn parse_create_holiday_calendar(p: &mut Parser) -> Result<Stmt, DbError> {
    // expect string literal → country_code
    let country_code = p.expect_string_literal()?;
    // expect '('
    p.expect(&Token::LParen)?;
    let mut date_literals = Vec::new();
    // parse comma-separated string literals until ')'
    while !matches!(p.peek(), Token::RParen | Token::Eof) {
        date_literals.push(p.expect_string_literal()?);
        if !p.eat(&Token::Comma) { break; }
    }
    p.expect(&Token::RParen)?;
    Ok(Stmt::CreateHolidayCalendar(CreateHolidaryCalendarStmt {
        country_code,
        date_literals,
    }))
}

/// `DROP HOLIDAY CALENDAR [IF EXISTS] 'code'`
pub(crate) fn parse_drop_holiday_calendar(p: &mut Parser) -> Result<Stmt, DbError> {
    let if_exists = eat_if_exists(p)?;
    let country_code = p.expect_string_literal()?;
    Ok(Stmt::DropHolidayCalendar(DropHolidayCalendarStmt { country_code, if_exists }))
}
```

### Tests to add (parser level, in existing `integration_ddl_parser.rs`)

```rust
#[test]
fn parse_create_holiday_calendar_basic() {
    let sql = "CREATE HOLIDAY CALENDAR 'CO' ('2026-01-01', '2026-12-25')";
    let stmt = parse_one(sql).unwrap();
    assert!(matches!(stmt, Stmt::CreateHolidayCalendar(_)));
    if let Stmt::CreateHolidayCalendar(s) = stmt {
        assert_eq!(s.country_code, "CO");
        assert_eq!(s.date_literals, vec!["2026-01-01", "2026-12-25"]);
    }
}

#[test]
fn parse_create_holiday_calendar_empty() {
    let sql = "CREATE HOLIDAY CALENDAR 'XX' ()";
    let stmt = parse_one(sql).unwrap();
    if let Stmt::CreateHolidayCalendar(s) = stmt {
        assert!(s.date_literals.is_empty());
    }
}

#[test]
fn parse_drop_holiday_calendar() {
    let sql = "DROP HOLIDAY CALENDAR 'CO'";
    let stmt = parse_one(sql).unwrap();
    assert!(matches!(stmt, Stmt::DropHolidayCalendar(s) if !s.if_exists));
}

#[test]
fn parse_drop_holiday_calendar_if_exists() {
    let sql = "DROP HOLIDAY CALENDAR IF EXISTS 'CO'";
    let stmt = parse_one(sql).unwrap();
    assert!(matches!(stmt, Stmt::DropHolidayCalendar(s) if s.if_exists));
}
```

### Verification

```bash
./tools/vm.sh test -- -p axiomdb-sql --test integration_ddl_parser 2>&1 | tail -10
./tools/vm.sh clippy 2>&1 | tail -5
```

### Commit

```
feat(fase-20): 20.16 step 2 — AST + parser for CREATE/DROP HOLIDAY CALENDAR

- ast.rs: CreateHolidayCalendarStmt, DropHolidayCalendarStmt, two Stmt variants
- parser/mod.rs: dispatch HOLIDAY CALENDAR in parse_create / parse_drop
- parser/ddl.rs: parse_create_holiday_calendar, parse_drop_holiday_calendar
- 4 parser tests in integration_ddl_parser.rs

Step 2 of specs/fase-20/plan-20.16-business-calendar.md
```

---

## Step 3 — DDL executor: CREATE + DROP

**Goal:** Execute `CREATE/DROP HOLIDAY CALENDAR` end-to-end.  
**Files:** `executor/ddl_holiday_calendar.rs` (new, included), `executor/mod.rs`, `executor/exec_dispatch.rs`

### 3a — ddl_holiday_calendar.rs

```rust
// crates/axiomdb-sql/src/executor/ddl_holiday_calendar.rs
// Included into the executor module (mod.rs) so it shares all imports.

use chrono::NaiveDate;

/// Validates country_code, parses date strings → i32 days-since-epoch,
/// deduplicates, sorts, then calls CatalogWriter::upsert_holiday_calendar.
fn execute_create_holiday_calendar(
    stmt: &CreateHolidayCalendarStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let code = validate_country_code(&stmt.country_code)?;

    // Parse each date literal → i32 days since epoch.
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
    let mut days: Vec<i32> = stmt.date_literals.iter().map(|s| {
        NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .map_err(|_| DbError::ParseError {
                message: format!("invalid date literal in HOLIDAY CALENDAR: '{s}'"),
                position: None,
            })
            .map(|d| (d - epoch).num_days() as i32)
    }).collect::<Result<Vec<_>, _>>()?;

    // Deduplicate + sort.
    days.sort_unstable();
    days.dedup();

    let def = HolidayCalendarDef { country_code: code.clone(), holidays: days };

    let mut conn = txn.begin()?;
    let mut writer = CatalogWriter::new(storage, txn, &mut conn)?;
    writer.upsert_holiday_calendar(def)?;
    if let Some(txn_id) = txn.commit(conn)? {
        txn.wal_flush_and_fsync()?;
        txn.advance_committed_single(txn_id);
    }

    // Invalidate session cache for this country.
    ctx.holiday_cache.remove(&code);

    Ok(QueryResult::Affected(0))
}

fn execute_drop_holiday_calendar(
    stmt: &DropHolidayCalendarStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let code = stmt.country_code.to_ascii_uppercase();

    let mut conn = txn.begin()?;
    let mut writer = CatalogWriter::new(storage, txn, &mut conn)?;
    let found = writer.delete_holiday_calendar(&code)?;
    if let Some(txn_id) = txn.commit(conn)? {
        txn.wal_flush_and_fsync()?;
        txn.advance_committed_single(txn_id);
    }

    if !found && !stmt.if_exists {
        return Err(DbError::ObjectNotFound {
            object: format!("holiday calendar '{code}'"),
        });
    }

    ctx.holiday_cache.remove(&code);
    Ok(QueryResult::Affected(0))
}

fn validate_country_code(raw: &str) -> Result<String, DbError> {
    if raw.is_empty() || raw.len() > 16 || raw.contains(|c: char| !c.is_alphanumeric() && c != '_' && c != '-') {
        return Err(DbError::InvalidValue {
            reason: format!("invalid country code '{raw}': must be 1-16 alphanumeric chars"),
        });
    }
    Ok(raw.to_ascii_uppercase())
}
```

### 3b — exec_dispatch.rs: wire new Stmt variants

In the large `match stmt { ... }` block:
```rust
Stmt::CreateHolidayCalendar(s) => {
    execute_create_holiday_calendar(s, storage, txn, ctx)
}
Stmt::DropHolidayCalendar(s) => {
    execute_drop_holiday_calendar(s, storage, txn, ctx)
}
```

### 3c — executor/mod.rs: add include!

```rust
include!("ddl_holiday_calendar.rs");
```

### Tests to add (in integration_business_calendar.rs)

DDL lifecycle tests only at this point — no function tests yet.

```rust
#[test]
fn create_holiday_calendar_basic() { ... }        // CREATE succeeds, Affected(0)
#[test]
fn create_holiday_calendar_replace() { ... }      // second CREATE for same code OK
#[test]
fn create_holiday_calendar_empty_dates() { ... }  // () is valid
#[test]
fn create_invalid_date_literal() { ... }          // 'not-a-date' → ParseError
#[test]
fn drop_holiday_calendar_basic() { ... }          // DROP succeeds
#[test]
fn drop_holiday_calendar_if_exists_missing() { ... }  // IF EXISTS, not found → OK
#[test]
fn drop_holiday_calendar_not_found_error() { ... }    // no IF EXISTS → ObjectNotFound
```

### Verification

```bash
./tools/vm.sh test -- -p axiomdb-sql --test integration_business_calendar 2>&1 | tail -10
./tools/vm.sh clippy 2>&1 | tail -5
```

### Commit

```
feat(fase-20): 20.16 step 3 — DDL executor for CREATE/DROP HOLIDAY CALENDAR

- ddl_holiday_calendar.rs: execute_create_holiday_calendar (parse dates, upsert),
  execute_drop_holiday_calendar (delete, IF EXISTS), validate_country_code
- exec_dispatch.rs: match arms for CreateHolidayCalendar + DropHolidayCalendar
- 7 DDL integration tests

Step 3 of specs/fase-20/plan-20.16-business-calendar.md
```

---

## Step 4 — Session cache + scalar functions

**Goal:** Implement `IS_BUSINESS_DAY`, `NEXT_BUSINESS_DAY`, `BUSINESS_DAYS_BETWEEN`.  
**Files:** `session.rs`, `executor/business_calendar_runtime.rs` (new), `executor/mod.rs`, `executor/exec_subquery.rs`

### 4a — session.rs: holiday_cache field

```rust
// In SessionContext struct, after sequence_currvals:
/// Per-session cache of holiday sets keyed by upper-cased country code (Phase 20.16).
///
/// Populated on first function call for a country; invalidated on CREATE/DROP
/// HOLIDAY CALENDAR for the affected country. Arc<HashSet> allows sharing the
/// set by reference across multiple calls in the same expression.
pub holiday_cache: HashMap<String, std::sync::Arc<std::collections::HashSet<i32>>>,
```

Initialize to `HashMap::new()` in `SessionContext::new()`.

### 4b — business_calendar_runtime.rs

```rust
// Included into executor/mod.rs so it shares all imports.
// Pattern: same as sequence_runtime.rs / cron_runtime.rs.

use std::{collections::HashSet, sync::Arc};
use chrono::NaiveDate;

fn eval_business_calendar_function(
    name: &str,
    args: &[Expr],
    row: &[Value],
    runner: &mut ExecSubqueryRunner<'_>,
) -> Result<Option<Value>, DbError> {
    match name.to_ascii_lowercase().as_str() {
        "is_business_day" => is_business_day_fn(args, row, runner).map(Some),
        "next_business_day" => next_business_day_fn(args, row, runner).map(Some),
        "business_days_between" => business_days_between_fn(args, row, runner).map(Some),
        _ => Ok(None),
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn load_calendar(
    country_upper: &str,
    runner: &mut ExecSubqueryRunner<'_>,
) -> Result<Arc<HashSet<i32>>, DbError> {
    // Cache hit
    if let Some(cached) = runner.ctx.holiday_cache.get(country_upper) {
        return Ok(Arc::clone(cached));
    }
    // Cache miss: read from catalog
    let mut conn = runner.txn.begin()?;
    let snap = runner.txn.active_snapshot(&conn);
    let mut reader = CatalogReader::new(runner.storage, snap)?;
    let set: HashSet<i32> = reader
        .get_holiday_calendar(country_upper)?
        .map(|def| def.holidays.into_iter().collect())
        .unwrap_or_default();
    runner.txn.rollback(conn)?; // read-only, no changes to commit
    let arc = Arc::new(set);
    runner.ctx.holiday_cache.insert(country_upper.to_string(), Arc::clone(&arc));
    Ok(arc)
}

fn is_weekday(days_since_epoch: i32) -> bool {
    // (days_since_epoch + 4) % 7: 0=Mon 1=Tue 2=Wed 3=Thu 4=Fri 5=Sat 6=Sun
    let wd = ((days_since_epoch + 4).rem_euclid(7)) as u32;
    wd < 5 // Mon-Fri
}

fn coerce_to_days(v: &Value) -> Option<i32> {
    match v {
        Value::Date(d) => Some(*d),
        Value::Timestamp(us) => Some((*us / 86_400_000_000) as i32),
        Value::Text(s) => {
            let epoch = NaiveDate::from_ymd_opt(1970, 1, 1)?;
            NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
                .map(|d| (d - epoch).num_days() as i32)
        }
        _ => None,
    }
}

// ── IS_BUSINESS_DAY(date, country_code) → BOOL ──────────────────────────────

fn is_business_day_fn(
    args: &[Expr],
    row: &[Value],
    runner: &mut ExecSubqueryRunner<'_>,
) -> Result<Value, DbError> {
    if args.len() != 2 {
        return Err(DbError::TypeMismatch {
            expected: "is_business_day(date, country_code TEXT)".into(),
            got: format!("{} args", args.len()),
        });
    }
    let date_val = crate::eval::eval_with(&args[0], row, runner)?;
    let cc_val   = crate::eval::eval_with(&args[1], row, runner)?;

    if matches!(date_val, Value::Null) || matches!(cc_val, Value::Null) {
        return Ok(Value::Null);
    }
    let days = coerce_to_days(&date_val).ok_or_else(|| DbError::TypeMismatch {
        expected: "DATE or TIMESTAMP".into(),
        got: date_val.to_string(),
    })?;
    let Value::Text(cc) = cc_val else {
        return Err(DbError::TypeMismatch {
            expected: "TEXT country_code".into(),
            got: cc_val.to_string(),
        });
    };
    if !is_weekday(days) {
        return Ok(Value::Bool(false));
    }
    let holidays = load_calendar(&cc.to_ascii_uppercase(), runner)?;
    Ok(Value::Bool(!holidays.contains(&days)))
}

// ── NEXT_BUSINESS_DAY(date, country_code) → DATE ────────────────────────────

fn next_business_day_fn(
    args: &[Expr],
    row: &[Value],
    runner: &mut ExecSubqueryRunner<'_>,
) -> Result<Value, DbError> {
    if args.len() != 2 { ... }
    let date_val = crate::eval::eval_with(&args[0], row, runner)?;
    let cc_val   = crate::eval::eval_with(&args[1], row, runner)?;
    if matches!(date_val, Value::Null) || matches!(cc_val, Value::Null) {
        return Ok(Value::Null);
    }
    let start = coerce_to_days(&date_val).ok_or_else(|| ...)?;
    let Value::Text(cc) = cc_val else { return Err(...); };
    let holidays = load_calendar(&cc.to_ascii_uppercase(), runner)?;

    for offset in 1i32..=14 {
        let candidate = start.checked_add(offset).ok_or_else(|| DbError::InvalidValue {
            reason: "date out of representable range".into(),
        })?;
        if is_weekday(candidate) && !holidays.contains(&candidate) {
            return Ok(Value::Date(candidate));
        }
    }
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
    let date_str = (epoch + chrono::Duration::days(start as i64))
        .format("%Y-%m-%d").to_string();
    Err(DbError::InvalidValue {
        reason: format!("NEXT_BUSINESS_DAY: no business day found within 14 days after {date_str}"),
    })
}

// ── BUSINESS_DAYS_BETWEEN(date1, date2, country_code) → INT ─────────────────

fn business_days_between_fn(
    args: &[Expr],
    row: &[Value],
    runner: &mut ExecSubqueryRunner<'_>,
) -> Result<Value, DbError> {
    if args.len() != 3 { ... }
    // eval + NULL propagation
    // sign = 1 if d1 < d2, -1 if d1 > d2, 0 if equal
    // Mathematical shortcut:
    //   total_days = |d2 - d1|
    //   full_weeks = total_days / 7
    //   weekdays = full_weeks * 5 + count_weekdays_in_remainder
    //   holidays_in_range = holidays.iter().filter(|&&h| h >= lo && h < hi).count()
    //   result = sign * (weekdays - holidays_in_range)
    let count = count_business_days(d1, d2, &holidays); // sign handled inside
    Ok(Value::Int(count))
}

fn count_business_days(lo: i32, hi: i32, holidays: &HashSet<i32>) -> i32 {
    if lo == hi { return 0; }
    let (sign, lo, hi) = if lo < hi { (1i32, lo, hi) } else { (-1, hi, lo) };
    let total = (hi - lo) as u32;
    let full_weeks = total / 7;
    let remainder = total % 7;
    // Count weekdays in the remainder starting at lo + full_weeks * 7
    let rem_start = lo + (full_weeks * 7) as i32;
    let rem_weekdays: i32 = (0..remainder as i32)
        .filter(|&i| is_weekday(rem_start + i))
        .count() as i32;
    let weekdays = (full_weeks * 5) as i32 + rem_weekdays;
    // Subtract holidays in [lo, hi)
    let holiday_count = holidays.iter()
        .filter(|&&h| h >= lo && h < hi)
        .count() as i32;
    sign * (weekdays - holiday_count)
}
```

### 4c — exec_subquery.rs: wire into eval_function

```rust
fn eval_function(
    &mut self,
    name: &str,
    args: &[Expr],
    row: &[Value],
) -> Result<Option<Value>, DbError> {
    if let Some(v) = eval_sequence_function(name, args, row, self)? {
        return Ok(Some(v));
    }
    if let Some(v) = eval_cron_function(name, args, row, self)? {
        return Ok(Some(v));
    }
    eval_business_calendar_function(name, args, row, self)
}
```

### 4d — executor/mod.rs

```rust
include!("business_calendar_runtime.rs");
```

### Verification

```bash
./tools/vm.sh test -- -p axiomdb-sql --test integration_business_calendar 2>&1 | tail -10
./tools/vm.sh clippy 2>&1 | tail -5
```

### Commit

```
feat(fase-20): 20.16 step 4 — scalar functions IS_BUSINESS_DAY, NEXT_BUSINESS_DAY, BUSINESS_DAYS_BETWEEN

- session.rs: holiday_cache: HashMap<String, Arc<HashSet<i32>>>
- business_calendar_runtime.rs: eval_business_calendar_function dispatches all 3
  functions; load_calendar with session-level Arc<HashSet> cache; is_weekday helper;
  count_business_days with O(1) week math + HashSet holiday subtraction
- exec_subquery.rs: chain eval_business_calendar_function in eval_function

Step 4 of specs/fase-20/plan-20.16-business-calendar.md
```

---

## Step 5 — Full integration tests + wire smoke

**Goal:** Cover all spec edge cases; add wire assertions.  
**Files:** `crates/axiomdb-sql/tests/integration_business_calendar.rs`, `tools/wire-test.py`

### Tests to add (complete list)

```rust
// ── DDL lifecycle (carried from step 3) ──────────────────────────────────────
// test_create_holiday_calendar_basic
// test_create_holiday_calendar_replace
// test_create_holiday_calendar_empty_dates
// test_create_invalid_date_literal
// test_drop_holiday_calendar_basic
// test_drop_holiday_calendar_if_exists_missing
// test_drop_holiday_calendar_not_found_error

// ── IS_BUSINESS_DAY ──────────────────────────────────────────────────────────
fn test_is_business_day_weekday_non_holiday()
    // Monday 2026-01-05 with CO calendar → TRUE
fn test_is_business_day_weekend()
    // Saturday 2026-01-03 → FALSE (always, regardless of calendar)
fn test_is_business_day_holiday()
    // Thursday 2026-01-01 with CO calendar containing 2026-01-01 → FALSE
fn test_is_business_day_no_calendar()
    // 'XX' calendar not created → weekday → TRUE; Saturday → FALSE
fn test_is_business_day_null_date()
    // IS_BUSINESS_DAY(NULL, 'CO') → NULL
fn test_is_business_day_null_country()
    // IS_BUSINESS_DAY('2026-01-05', NULL) → NULL
fn test_is_business_day_duplicate_dates_in_create()
    // CREATE HOLIDAY CALENDAR 'DUP' ('2026-01-01', '2026-01-01') — stored once
fn test_is_business_day_drop_then_call()
    // After DROP → weekend-only (no error)

// ── NEXT_BUSINESS_DAY ────────────────────────────────────────────────────────
fn test_next_business_day_skip_weekend()
    // Friday 2026-01-02 → Monday 2026-01-05 (3 days forward)
fn test_next_business_day_skip_holiday()
    // Thursday 2026-01-01 (CO holiday) → Friday 2026-01-02
fn test_next_business_day_null_propagation()
    // NULL date or NULL country → NULL
fn test_next_business_day_14day_limit()
    // Calendar with 14 consecutive non-weekdays → InvalidValue

// ── BUSINESS_DAYS_BETWEEN ────────────────────────────────────────────────────
fn test_business_days_between_forward()
    // 2026-01-01 .. 2026-01-08 with CO (1 holiday Fri, Sat, Sun excluded) → 4
fn test_business_days_between_backward()
    // 2026-01-08 .. 2026-01-01 → -4
fn test_business_days_between_same_day()
    // d1 == d2 → 0
fn test_business_days_between_null_propagation()
    // any NULL arg → NULL
fn test_business_days_between_large_range()
    // 2026-01-01 .. 2027-01-01 (1 year) → correct count
```

Total: **~20 tests** in `integration_business_calendar.rs`.

### Wire smoke assertions

```python
# tools/wire-test.py — block [20.16 business calendar]
conn.query("CREATE HOLIDAY CALENDAR 'CO' ('2026-01-01', '2026-12-25')")
assert_one(conn.query("SELECT IS_BUSINESS_DAY('2026-01-01', 'CO')"),
           "is_business_day holiday", [[0]])  # holiday → FALSE
assert_one(conn.query("SELECT IS_BUSINESS_DAY('2026-01-02', 'CO')"),
           "is_business_day weekday", [[1]])  # Friday, not holiday → TRUE
assert_one(conn.query("SELECT NEXT_BUSINESS_DAY('2026-01-02', 'CO')"),
           "next_business_day", [["2026-01-05"]])  # skip weekend
assert_one(conn.query("SELECT BUSINESS_DAYS_BETWEEN('2026-01-01', '2026-01-08', 'CO')"),
           "business_days_between", [[4]])
```

### Verification (full workspace)

```bash
./tools/vm.sh test -- --workspace 2>&1 | tail -10
./tools/vm.sh clippy 2>&1 | tail -5
./tools/vm.sh fmt-check 2>&1 | tail -5
# wire smoke after fresh binary:
pkill axiomdb-server || true
cargo build -p axiomdb-server --release 2>&1 | tail -5
python3 tools/wire-test.py 2>&1 | tail -10
```

### Commit

```
feat(fase-20): 20.16 step 5 — integration tests + wire smoke

- integration_business_calendar.rs: 20 tests covering DDL lifecycle,
  IS_BUSINESS_DAY (weekday/weekend/holiday/NULL/no-calendar),
  NEXT_BUSINESS_DAY (weekend-skip/holiday-skip/NULL/14-day-limit),
  BUSINESS_DAYS_BETWEEN (forward/backward/same-day/NULL/large-range)
- wire-test.py: 4 assertions for [20.16 business calendar]

Step 5 of specs/fase-20/plan-20.16-business-calendar.md
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|---|---|---|
| `expect_ident_ci("calendar")` helper doesn't exist in parser | medium | Check parser API; use `eat_ident_ci` or inline match |
| `txn.rollback(conn)` for read-only calendar load throws | low | Check sequence_runtime — it uses `txn.commit()`; align approach |
| `ObjectNotFound` variant doesn't carry a free string | low | Check `DbError` — add `object: String` field or use `InvalidValue` |
| Meta page body offset 184 conflicts with unreleased field | low | Grep meta.rs for all constants before writing |

## Rollback plan

1. `git reset --hard <commit before step 1>` to discard all 5 steps, OR
2. Leave on current branch; mark spec status `draft` with failure note.

## Estimated effort

Total: ~6–8 hours  
Step 1: 1.5h (catalog boilerplate, 3 files)  
Step 2: 1h (AST + parser, pattern well established)  
Step 3: 1.5h (DDL executor, date parsing, cache invalidation)  
Step 4: 2h (function logic, cache, math, wiring)  
Step 5: 1h (test coverage + wire smoke)
