# Plan: tablesample

Phase: 20 — Types + import/export
Task: TABLESAMPLE — page-level and row-level random sampling
Spec: specs/fase-20/spec-tablesample.md
Status: in-progress

## Summary

Three steps, all confined to `axiomdb-sql`. Step 1 adds the AST types
(`TableSample`, `TableSampleMethod`) and extends `TableRef` with an optional
`tablesample` field. Step 2 wires the parser (`parse_from_item` in `dml.rs`)
to consume `TABLESAMPLE method(pct) [REPEATABLE(expr)]` after the optional alias,
being careful to exclude `TABLESAMPLE` from implicit-alias detection. Step 3 adds
`scan_table_sampled` to `table_scan.rs` (SYSTEM = per-page coin flip, BERNOULLI =
per-row coin flip) and hooks it into the `Scan` arm of `select_core.rs`, with a
post-scan filter fallback for clustered tables and non-Scan access methods.
Integration tests and wire smoke tests close the subphase.

## Dependencies

Must be done first:
- [x] spec-tablesample.md approved

Blocks:
- nothing

## Affected files

New files:
- `crates/axiomdb-sql/tests/integration_tablesample.rs` — integration tests

Modified files:
- `crates/axiomdb-sql/src/ast.rs` — `TableSampleMethod`, `TableSample`, extend `TableRef`
- `crates/axiomdb-sql/src/parser/dml.rs` — `parse_optional_tablesample`, hook into `parse_from_item`
- `crates/axiomdb-sql/src/table_scan.rs` — `scan_table_sampled`
- `crates/axiomdb-sql/src/executor/select_core.rs` — use sampled scan when tablesample is set
- `tools/wire-test.py` — 2+ new assertions
- `docs/progreso.md` — mark 20.11 ✅
- `docs-site/src/user-guide/sql-reference/expressions.md` — TABLESAMPLE note
- `memory/project_state.md` — update state

---

## Step 1 — AST: TableSample types + TableRef extension

**Goal:** Add the data structures so the parser and executor have types to fill in.
**Files:** `crates/axiomdb-sql/src/ast.rs`

### Changes

```rust
// After the existing imports, before pub struct TableRef

/// `TABLESAMPLE SYSTEM(p)` or `TABLESAMPLE BERNOULLI(p)` on a table reference.
#[derive(Debug, Clone, PartialEq)]
pub struct TableSample {
    pub method: TableSampleMethod,
    /// Percentage in [0.0, 100.0].
    pub percent: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TableSampleMethod {
    System,
    Bernoulli,
}
```

Add the field to `TableRef`:

```rust
pub struct TableRef {
    pub database: Option<String>,
    pub schema: Option<String>,
    pub name: String,
    pub alias: Option<String>,
    pub tablesample: Option<TableSample>,   // NEW — None means no sampling
}
```

Update `TableRef::simple` to set `tablesample: None`.

### Verification

```bash
./tools/vm.sh clippy -p axiomdb-sql
```

### Commit

```
feat(fase-20): step 1 — AST: TableSample + TableRef.tablesample field
```

---

## Step 2 — Parser: TABLESAMPLE method(pct) [REPEATABLE]

**Goal:** Parse `TABLESAMPLE SYSTEM(p)` / `BERNOULLI(p)` after alias in FROM;
exclude `TABLESAMPLE` from implicit-alias detection; reject REPEATABLE at parse time
with `DbError::NotImplemented`.

**Files:** `crates/axiomdb-sql/src/parser/dml.rs`

### Changes

Add a helper at the bottom of `dml.rs` (near `peek_ident_ci_at`):

```rust
fn is_tablesample_start(p: &Parser) -> bool {
    peek_ident_ci_at(p, 0, "TABLESAMPLE")
}

/// Parses `TABLESAMPLE SYSTEM(p)` or `BERNOULLI(p)` if present.
/// Returns `Err` for unknown methods, out-of-range percent, or REPEATABLE.
fn parse_optional_tablesample(p: &mut Parser) -> Result<Option<TableSample>, DbError> {
    if !is_tablesample_start(p) {
        return Ok(None);
    }
    p.advance(); // consume TABLESAMPLE

    // method name: SYSTEM or BERNOULLI (parsed as plain identifier)
    let method_name = p.parse_identifier()?;
    let method = match method_name.to_ascii_uppercase().as_str() {
        "SYSTEM" => TableSampleMethod::System,
        "BERNOULLI" => TableSampleMethod::Bernoulli,
        other => {
            return Err(DbError::ParseError {
                message: format!(
                    "unknown TABLESAMPLE method '{}'; expected SYSTEM or BERNOULLI",
                    other
                ),
            })
        }
    };

    p.expect(&Token::LParen)?;
    let pct_expr = parse_expr(p)?; // constant numeric expression
    p.expect(&Token::RParen)?;

    // Evaluate to f64 (must be a literal or simple numeric constant).
    let percent = match pct_expr {
        crate::expr::Expr::Literal(axiomdb_types::Value::Int(n)) => n as f64,
        crate::expr::Expr::Literal(axiomdb_types::Value::BigInt(n)) => n as f64,
        crate::expr::Expr::Literal(axiomdb_types::Value::Real(f)) => f,
        other => {
            return Err(DbError::ParseError {
                message: format!(
                    "TABLESAMPLE percent must be a numeric literal, got {:?}",
                    other
                ),
            })
        }
    };

    if !(0.0..=100.0).contains(&percent) {
        return Err(DbError::InvalidValue {
            reason: "TABLESAMPLE percent must be in [0, 100]".into(),
        });
    }

    // REPEATABLE(seed) — accepted syntactically, rejected semantically.
    if peek_ident_ci_at(p, 0, "REPEATABLE") {
        p.advance(); // REPEATABLE
        p.expect(&Token::LParen)?;
        parse_expr(p)?; // consume and discard seed
        p.expect(&Token::RParen)?;
        return Err(DbError::NotImplemented {
            feature: "TABLESAMPLE REPEATABLE".into(),
        });
    }

    Ok(Some(TableSample { method, percent }))
}
```

Modify `parse_from_item` (lines 1027–1031):

```rust
// Before (line 1027):
if p.eat(&Token::As) || (is_implicit_alias_token(p.peek()) && !is_pivot_clause_start(p)) {

// After:
if p.eat(&Token::As) || (is_implicit_alias_token(p.peek()) && !is_pivot_clause_start(p) && !is_tablesample_start(p)) {
```

Then insert after the alias block and before `parse_optional_pivot_clause`:

```rust
// Optional TABLESAMPLE clause (parsed after alias, before PIVOT).
table_ref.tablesample = parse_optional_tablesample(p)?;

parse_optional_pivot_clause(p, FromClause::Table(table_ref))
```

### Parser tests (integration_tablesample.rs — parser shape only)

```rust
fn eval_err(sql: &str) -> DbError { ... }

#[test]
fn tablesample_system_parses() {
    // runs without error
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t_sys (v INT)", &mut storage, &mut txn);
    run("SELECT v FROM t_sys TABLESAMPLE SYSTEM(10)", &mut storage, &mut txn);
}

#[test]
fn tablesample_bernoulli_parses() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t_bern (v INT)", &mut storage, &mut txn);
    run("SELECT v FROM t_bern TABLESAMPLE BERNOULLI(10)", &mut storage, &mut txn);
}

#[test]
fn tablesample_unknown_method_errors() {
    let err = eval_err_table("SELECT v FROM t TABLESAMPLE RESERVOIR(10)");
    assert!(matches!(err, DbError::ParseError { .. }));
}

#[test]
fn tablesample_negative_pct_errors() {
    let err = eval_err_table("SELECT v FROM t TABLESAMPLE SYSTEM(-1)");
    assert!(matches!(err, DbError::InvalidValue { .. }));
}

#[test]
fn tablesample_pct_over_100_errors() {
    let err = eval_err_table("SELECT v FROM t TABLESAMPLE SYSTEM(101)");
    assert!(matches!(err, DbError::InvalidValue { .. }));
}

#[test]
fn tablesample_repeatable_errors() {
    let err = eval_err_table("SELECT v FROM t TABLESAMPLE SYSTEM(10) REPEATABLE(42)");
    assert!(matches!(err, DbError::NotImplemented { .. }));
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql --test integration_tablesample
./tools/vm.sh clippy -p axiomdb-sql
```

### Commit

```
feat(fase-20): step 2 — parser: TABLESAMPLE SYSTEM/BERNOULLI(pct) + REPEATABLE stub
```

---

## Step 3 — Scan layer + Executor + Close

**Goal:** `scan_table_sampled` applies SYSTEM (page-level) and BERNOULLI (row-level)
sampling; `select_core.rs` uses it; workspace tests + wire smoke pass.

**Files:** `crates/axiomdb-sql/src/table_scan.rs`,
`crates/axiomdb-sql/src/executor/select_core.rs`,
`crates/axiomdb-sql/tests/integration_tablesample.rs` (remaining tests),
`tools/wire-test.py`, `docs/progreso.md`,
`docs-site/src/user-guide/sql-reference/expressions.md`,
`memory/project_state.md`

### `scan_table_sampled` in `table_scan.rs`

Add to `impl TableEngine` (after `scan_table_direct`):

```rust
/// Scans a heap table, sampling rows according to `sample`.
///
/// SYSTEM: flips a coin per page — include all visible rows or skip all rows.
/// BERNOULLI: flips a coin per visible row.
/// Shortcuts: percent >= 100 → full scan; percent <= 0 → empty immediately.
/// Clustered tables: caller is responsible for applying a post-scan Bernoulli filter.
pub fn scan_table_sampled(
    storage: &dyn StorageEngine,
    table_def: &TableDef,
    columns: &[ColumnDef],
    snap: TransactionSnapshot,
    column_mask: Option<&[bool]>,
    sample: &crate::ast::TableSample,
) -> Result<Vec<(RecordId, Vec<Value>)>, DbError> {
    use crate::ast::TableSampleMethod;
    use rand::Rng;

    // Shortcuts
    if sample.percent <= 0.0 {
        return Ok(Vec::new());
    }
    if sample.percent >= 100.0 {
        return Self::scan_table_direct(storage, table_def, columns, snap, column_mask);
    }

    ensure_heap_table(table_def, "TABLESAMPLE on clustered table — use post-scan filter")?;

    let threshold = sample.percent / 100.0;
    let col_types = column_data_types(columns);
    let masked_decode = column_mask.filter(|mask| !mask.iter().all(|&b| b));
    let mut result = Vec::new();
    let mut rng = rand::thread_rng();
    let mut current = table_def.root_page_id;

    while current != 0 {
        let raw = *storage.read_page(current)?.as_bytes();
        let page = Page::from_bytes(raw)?;
        let next = heap_chain::chain_next_page(&page);

        // SYSTEM: skip this page entirely with probability (1 - threshold).
        if sample.method == TableSampleMethod::System && rng.gen::<f64>() >= threshold {
            current = next;
            continue;
        }

        if next != 0 {
            storage.prefetch_hint(next, 8);
        }

        let num = num_slots(&page);
        for slot_id in 0..num {
            let entry = read_slot(&page, slot_id);
            if entry.is_dead() {
                continue;
            }
            let off = entry.offset as usize;
            let len = entry.length as usize;
            let bytes = &page.as_bytes()[off..off + len];
            let header: &RowHeader = bytemuck::from_bytes(&bytes[..size_of::<RowHeader>()]);
            if !header.is_visible(&snap) {
                continue;
            }
            // BERNOULLI: flip coin per row.
            if sample.method == TableSampleMethod::Bernoulli && rng.gen::<f64>() >= threshold {
                continue;
            }
            let row_data = &bytes[size_of::<RowHeader>()..];
            let mut values = if let Some(mask) = masked_decode {
                decode_row_masked(row_data, &col_types, mask)?
            } else {
                decode_row(row_data, &col_types)?
            };
            detoast_row(&mut values, storage);
            result.push((RecordId { page_id: current, slot_id }, values));
        }

        current = next;
    }

    Ok(result)
}
```

### `select_core.rs` — hook into Scan arm

After `let from_table_ref = match stmt.from.take() { ... }` (around line 112),
save the tablesample before it's borrowed:

```rust
let tablesample = from_table_ref.tablesample.clone();
```

In the `raw_rows` match block, replace the `AccessMethod::Scan` arms:

```rust
crate::planner::AccessMethod::Scan if resolved.def.is_clustered() => {
    let mut rows = crate::table::scan_clustered_table(
        storage, &resolved.def, &resolved.columns, snap,
    )?;
    // Post-scan Bernoulli filter for clustered tables.
    if let Some(s) = &tablesample {
        if s.percent <= 0.0 {
            rows.clear();
        } else if s.percent < 100.0 {
            use rand::Rng;
            let threshold = s.percent / 100.0;
            let mut rng = rand::thread_rng();
            rows.retain(|_| rng.gen::<f64>() < threshold);
        }
    }
    rows
}
crate::planner::AccessMethod::Scan => {
    if let Some(s) = &tablesample {
        TableEngine::scan_table_sampled(
            storage, &resolved.def, &resolved.columns, snap, None, s,
        )?
    } else {
        TableEngine::scan_table(storage, &resolved.def, &resolved.columns, snap, None)?
    }
}
```

### Integration tests (remaining in integration_tablesample.rs)

```rust
#[test]
fn tablesample_system_zero_returns_empty() { ... }

#[test]
fn tablesample_system_100_returns_all() { ... }

#[test]
fn tablesample_bernoulli_100_returns_all() { ... }

#[test]
fn tablesample_bernoulli_0_returns_empty() { ... }

#[test]
fn tablesample_system_returns_subset() {
    // 100 rows, SYSTEM(50) → result.len() is 0..=100 (non-deterministic)
    // but must be a subset of the full table values
}

#[test]
fn tablesample_with_where_and_limit() { ... }

#[test]
fn tablesample_alias_works() {
    // SELECT t.v FROM t AS t TABLESAMPLE SYSTEM(100)
}
```

### Wire assertions (tools/wire-test.py)

```python
# ── 20.11 — TABLESAMPLE ───────────────────────────────────────────────────────

cur.execute("CREATE TABLE IF NOT EXISTS _wire_ts (v INT)")
cur.execute("DELETE FROM _wire_ts")
for _i in range(100):
    cur.execute(f"INSERT INTO _wire_ts VALUES ({_i})")
conn.commit()

# 20.11a: SYSTEM(100) returns all rows
cur.execute("SELECT COUNT(*) FROM (SELECT v FROM _wire_ts TABLESAMPLE SYSTEM(100)) sub")
ok("[20.11a tablesample_system_100] SYSTEM(100) returns all 100 rows", cur.fetchone()[0] == 100)

# 20.11b: SYSTEM(0) returns no rows
cur.execute("SELECT COUNT(*) FROM (SELECT v FROM _wire_ts TABLESAMPLE SYSTEM(0)) sub")
ok("[20.11b tablesample_system_0] SYSTEM(0) returns 0 rows", cur.fetchone()[0] == 0)

# 20.11c: BERNOULLI(100) returns all rows
cur.execute("SELECT COUNT(*) FROM (SELECT v FROM _wire_ts TABLESAMPLE BERNOULLI(100)) sub")
ok("[20.11c tablesample_bernoulli_100] BERNOULLI(100) returns all rows", cur.fetchone()[0] == 100)
```

### Verification

```bash
./tools/vm.sh test --workspace
./tools/vm.sh clippy --workspace -- -D warnings
./tools/vm.sh fmt --check
```

### Commit

```
feat(fase-20): complete subphase 20.11 — TABLESAMPLE SYSTEM + BERNOULLI

- AST: TableSample, TableSampleMethod, TableRef.tablesample
- Parser: TABLESAMPLE method(pct) in parse_from_item; TABLESAMPLE excluded
  from implicit alias detection; REPEATABLE → NotImplemented
- scan_table_sampled: SYSTEM (per-page coin flip) + BERNOULLI (per-row coin flip)
- select_core.rs: sampled scan for heap Scan arm; post-scan filter for clustered
- Tests: N integration tests
- Wire: 577+/577+ assertions
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| `TABLESAMPLE` parsed as implicit alias | low | `is_tablesample_start` guard already in plan |
| SYSTEM on single-page table returns 0 rows with bad luck | low | acceptable — non-deterministic by spec |
| `tablesample` field breaks existing `TableRef` construction sites | medium | `TableRef::simple` sets it to `None`; struct init sites need `tablesample: None` |

## Rollback plan

1. `git reset --hard HEAD~N` (N commits from Step 1 onward), or
2. Branch `abandoned/plan-tablesample-<date>` + spec status → `draft`.

## Estimated effort

Total: ~1.5 hours
Per step: step 1: 15min, step 2: 35min, step 3: 40min
