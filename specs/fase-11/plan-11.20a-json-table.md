# Plan: 11.20a — `JSON_TABLE` (flat, no NESTED PATH)

## Files to create/modify

### Create

- `crates/axiomdb-sql/src/json_table.rs` — single module owning the flat
  JSON_TABLE execution pipeline. Public surface:
  - `pub struct JsonTableSpec` (AST → executor-ready description; a
    `Vec<JsonTableColumnSpec>` + the compiled row path).
  - `pub struct JsonTableColumnSpec` — per-column lowered form (compiled
    `Vec<PathStep>`, declared `DataType`, `OnBehavior` for empty/error,
    `ColumnKind { Regular, Ordinality, Exists { on_error: ExistsOnError } }`).
  - `pub fn compile_json_table(ast: &ast::JsonTable) -> Result<JsonTableSpec, DbError>`
    — parses every `PATH` string once (so runtime reruns only walk, not
    parse); validates "exactly one FOR ORDINALITY"; rejects unsupported type.
  - `pub fn materialize_json_table(spec: &JsonTableSpec, doc_value: &Value)
      -> Result<Vec<Row>, DbError>` — the core row emitter.
  - `pub fn columns_of(spec: &JsonTableSpec) -> Vec<ColumnDef>` — produces
    the virtual-table column descriptors for `analyzer_bind`.

- `crates/axiomdb-sql/tests/integration_json_table.rs` — ≥ 14 integration
  tests (see "Tests to write" below).

### Modify

- `crates/axiomdb-sql/src/ast.rs`
  - Add `JsonTable` struct (doc `Expr`, row path `String`, columns
    `Vec<JsonTableColumn>`, alias `Option<String>`).
  - Add `JsonTableColumn` enum: `Regular { name, ty: DataType, path,
    on_empty: SqlJsonOnBehavior, on_error: SqlJsonOnBehavior }`,
    `Ordinality { name }`, `Exists { name, ty: DataType, path,
    on_error: SqlJsonOnBehavior }`.
  - Add variant `FromClause::JsonTable(Box<JsonTable>)` (boxed — `JsonTable`
    carries `Vec` and column types; keep `FromClause` size reasonable).
  - Extend `Display for FromClause` / expression printer if one exists
    (check `expr_to_sql`/`stmt_to_sql`).

- `crates/axiomdb-sql/src/parser/dml.rs`
  - In `parse_from_item`, add — **before** the `(` subquery dispatch — a
    peek branch: if `eat_ident_ci("JSON_TABLE")` succeeds, delegate to
    `parser::json_table::parse_json_table_call(p)`.
  - `parse_join_clauses` needs the same dispatch — it currently calls
    `parse_from_item`, so the single change in `parse_from_item` is enough.

- `crates/axiomdb-sql/src/parser/` — **new file** `json_table.rs`:
  - `pub fn parse_json_table_call(p: &mut Parser) -> Result<FromClause, DbError>`
    — expects `(`, parses `doc` expression via `parse_expr`, expects `,`,
    parses row-path string literal, expects `COLUMNS`, `(`, column list,
    `)`, `)`. Then optional `AS alias` / implicit-alias (reusing the
    existing helper from `dml.rs`).
  - `fn parse_column_def(p: &mut Parser) -> Result<JsonTableColumn, DbError>`
    — identifier `name`, then:
    - `FOR ORDINALITY` → `Ordinality`
    - `<type> PATH 'jsonpath' [ON EMPTY] [ON ERROR]` → `Regular`
    - `<type> EXISTS PATH 'jsonpath' [<bool> ON ERROR]` → `Exists`
  - `fn parse_on_behavior(p, default) -> SqlJsonOnBehavior` — reuses
    `NULL | ERROR | DEFAULT expr`, same shape as
    `parser/expr.rs::parse_on_empty_error_clause` (inspect for the
    existing helper; call it if reusable).

- `crates/axiomdb-sql/src/analyzer_bind.rs`
  - Extend `bound_from_clause` with a new match arm
    `FromClause::JsonTable(jt)`:
    - Build a `Vec<ColumnDef>` via `json_table::columns_of_ast(jt)` (thin
      analyzer-side helper that maps each `JsonTableColumn` → `ColumnDef`
      with declared `ColumnType` from the AST `DataType`).
    - Publish as a single `BoundTable { alias: jt.alias, name: alias,
      columns, col_offset }`.
  - The doc `Expr` is analyzed later at exec time (no catalog work here);
    but we **do** call `analyze_expr(&jt.doc, …)` against the *outer*
    scope so that column references inside `doc` (e.g. `u.tags`) get
    resolved early — consistent with how correlated subqueries are bound.

- `crates/axiomdb-sql/src/executor/select_ctx.rs`
  - Recognize `FromClause::JsonTable` alongside subquery: delegate to new
    `execute_select_json_table_source` in `select_core.rs` when joins is
    empty, or to the join path otherwise.

- `crates/axiomdb-sql/src/executor/select_core.rs`
  - New `fn execute_select_json_table_source(stmt, storage, txn, conn_txn,
    ctx)` — mirrors `execute_select_derived`:
    1. Take `FromClause::JsonTable(jt)` out of `stmt.from`.
    2. `compile_json_table(&jt)`.
    3. Evaluate `jt.doc` via `eval_expr_scalar` against an empty outer
       row (for the first-FROM case) → `Value`.
    4. Convert to `serde_json::Value` via `value_to_serde_json` / parse
       text; `Value::Null` → zero rows.
    5. `let rows = materialize_json_table(&spec, &doc)?;`
    6. Apply outer WHERE / GROUP BY / ORDER BY / LIMIT with the existing
       tail of `execute_select_derived` (extract it into a shared helper
       `finish_materialized_select` to avoid duplication).

- `crates/axiomdb-sql/src/executor/select_joins_ctx.rs`
  - Change the entry signature from `from_ref: TableRef` to
    `from: &FromClause`. Inside, dispatch:
    - `Table(t)` → existing scan path.
    - `Subquery { query, alias }` → lift the existing subquery-arm code
      from the JOIN loop into a helper and call it for LEFT as well.
    - `JsonTable(jt)` → `compile_json_table` + evaluate `doc` against
      an empty outer row + `materialize_json_table` + publish a
      derived-style `JoinSourceSchema`.
  - Inside the JOIN iteration, add the same `FromClause::JsonTable` arm
    alongside the existing Subquery arm at line 43 of
    `select_joins_ctx.rs`. Correlated JOIN (JSON_TABLE's `doc` depending
    on left-row columns) is **deferred to 11.20d**; today we evaluate
    against an empty row and error if a left-column reference surfaces.

- `crates/axiomdb-sql/src/display/…` (DDL / SQL printer)
  - Add a printer for `FromClause::JsonTable` used by SHOW CREATE VIEW /
    EXPLAIN / round-tripping. Grep for `fn from_clause_to_sql` / the
    existing subquery formatter and mirror it.

- `tools/wire-test.py`
  - Append a JSON_TABLE smoke block: basic shred, ordinality, DEFAULT ON
    EMPTY, join with base table, NULL doc → 0 rows, invalid JSON doc
    error.

- `docs-site/src/internals/sql-parser.md` — add JSON_TABLE grammar rule
  fragment.
- `docs-site/src/sql-reference/dml.md` or new page
  `sql-reference/functions/json_table.md` — user-facing syntax + examples.
- `docs-site/src/user-guide/features/catalog.md` — only if JSON_TABLE
  surfaces in INFORMATION_SCHEMA routine metadata (it does not for now).
- `docs/fase-11.md` — append subphase 11.20a summary.
- `docs/progreso.md` — flip 11.20 from ⏳ to 🔄 with 11.20a ✅ nested.
- `memory/project_state.md`, `memory/architecture.md` (if crate surface
  changes), `memory/lessons.md` (if we learn anything surprising).

## Algorithm / Data structure

### Compile phase (once per statement)

```rust
// Lowered from AST; PATH strings parsed once.
pub struct JsonTableSpec {
    pub alias: String,              // surfacing alias
    pub row_path: Vec<PathStep>,    // compiled row path
    pub columns: Vec<JsonTableColumnSpec>,
    pub ordinality_index: Option<usize>, // index into columns, if any
}

pub struct JsonTableColumnSpec {
    pub name: String,
    pub ty: DataType,
    pub kind: JsonTableColumnKind,
}

pub enum JsonTableColumnKind {
    Regular {
        path: Vec<PathStep>,
        on_empty: OnBehaviorCompiled,   // Null | Error | Default(Expr)
        on_error: OnBehaviorCompiled,
    },
    Ordinality,
    Exists {
        path: Vec<PathStep>,
        on_error: ExistsOnError,        // True | False | Unknown | Error
    },
}
```

### Execute phase (per invocation)

```text
fn materialize_json_table(spec, doc):
    let matches = execute_jsonpath_owned(doc, &spec.row_path)
    let mut rows = Vec::with_capacity(matches.len())
    for (ord, m) in matches.enumerate(starting 1):
        let mut row = Vec::with_capacity(spec.columns.len())
        for col in &spec.columns:
            match &col.kind:
                Ordinality => row.push(Value::BigInt(ord as i64))
                Regular { path, on_empty, on_error } => {
                    let hits = execute_jsonpath_owned(m, path)
                    let val = match hits.as_slice() {
                        [] => apply_on_behavior(on_empty, col.ty, outer_row)?
                        [v] => coerce_or_on_error(v, col.ty, on_error, outer_row)?
                        // multiple matches on scalar JSON_VALUE shape → ON ERROR
                        _ => apply_on_behavior(on_error, col.ty, outer_row)?
                    }
                    row.push(val)
                }
                Exists { path, on_error } => {
                    // JSONPath execution errors are rare in our engine
                    // (malformed paths are rejected at compile); but type
                    // errors during filter eval can surface. Wrap and map.
                    match execute_jsonpath_owned(m, path) {
                        Ok(hits) if !hits.is_empty() => row.push(Value::Boolean(true)),
                        Ok(_)                        => row.push(Value::Boolean(false)),
                        Err(_)                       => row.push(apply_exists_on_error(on_error)?)
                    }
                }
        rows.push(row)
    Ok(rows)
```

Notes:

- `apply_on_behavior(Default(expr), ty, outer_row)` evaluates the stored
  `Expr` with no subquery runner (the expression is required to be
  non-correlated for 11.20a — matches PG behavior of allowing constants
  and simple scalar expressions). Then coerce to `ty` via
  `axiomdb_types::coerce::coerce(..., CoercionMode::Strict)`.
- `coerce_or_on_error` maps a `serde_json::Value` → `Value` (via a small
  converter from 11.19a / `sql_to_serde_json`'s inverse) then coerces.
  Any `DbError::InvalidCoercion` routes to the `ON ERROR` handler.
- `materialize_json_table` is pure on `serde_json::Value`; no storage
  access, no async — safe to call inside either first-FROM or join paths.

### Parser guard order

In `parse_from_item` the dispatch must be:

1. Case-insensitive identifier `JSON_TABLE` **followed by** `(`. If the
   user has a table named `json_table`, `SELECT * FROM json_table;` must
   still resolve to that table — so the guard checks both: if the next
   token after `JSON_TABLE` is `(`, it's the function; else roll back
   and treat as identifier (parser bookmark/peek_n).
2. `(` → subquery (existing).
3. Identifier → table ref (existing).

## Implementation phases (within this subphase)

1. **AST + printer** — add types, extend `Display`/SQL serializer,
   compile and run `cargo build -p axiomdb-sql`. No behavior yet.
2. **Parser** — `parse_json_table_call` + rollback-safe identifier
   dispatch; add parser unit tests (round-trip: parse then print).
3. **Analyzer binding** — `bound_from_clause` arm + `columns_of_ast`.
   No runtime yet. Assert via a debug EXPLAIN pass.
4. **Executor (flat)** — `json_table.rs` compile + materialize; wire
   into `select_core::execute_select_json_table_source`.
5. **JOIN right-side** — extend `select_joins_ctx.rs` (lift subquery
   arm into a helper, add JsonTable arm).
6. **JOIN left-side** — generalize the first-FROM to accept any
   `FromClause`.
7. **Integration tests** — `tests/integration_json_table.rs`.
8. **Wire smoke** — append to `tools/wire-test.py`.
9. **Close protocol** — workspace tests, clippy, fmt, docs, progreso,
   memory, commit, push.

## Tests to write

`tests/integration_json_table.rs` — minimum 14 cases:

1. Shred array of scalars: `JSON_TABLE('[1,2,3]', '$[*]' COLUMNS (v INT PATH '$'))`.
2. Shred array of objects with two columns.
3. `FOR ORDINALITY` counts from 1, increments per emitted row.
4. `PATH` miss + `DEFAULT 0 ON EMPTY` → returns 0.
5. `PATH` miss + default `NULL ON EMPTY` → returns NULL.
6. `PATH` miss + `ERROR ON EMPTY` → raises.
7. Type mismatch (string → INT) + `NULL ON ERROR` → NULL.
8. Type mismatch + `ERROR ON ERROR` → raises.
9. `EXISTS PATH` returns TRUE / FALSE.
10. `EXISTS PATH` with `UNKNOWN ON ERROR` — craft a path the engine
    can't evaluate on a non-object → NULL.
11. `NULL` document → 0 rows, no error.
12. Text document with invalid JSON → `InvalidCoercion`.
13. `JOIN base_table ON true` — cartesian-like with JSON_TABLE on the
    right; verify row count + column ordering.
14. `WHERE` predicate referencing a JSON_TABLE column.
15. Duplicate `FOR ORDINALITY` → parse error.
16. Missing `COLUMNS` keyword → parse error.

Parser unit tests live in `crates/axiomdb-sql/src/parser/json_table.rs`:

- Round-trip print-and-reparse invariance.
- Identifier rollback: `SELECT * FROM json_table` without `(` still
  parses as table reference.

## Anti-patterns to avoid

- **Re-parsing the row path per row.** Compile once in `compile_json_table`.
- **Allocating a fresh `serde_json::Value` for every column.** Share the
  match reference (`&serde_json::Value`) across all columns of a row.
- **Silent fallback** when the document is a scalar vs. array. PG's
  iteration semantics: if the row path matches zero items, zero rows;
  if it matches one scalar, one row with that scalar as context. Do
  not special-case `Null` to mean "treat as empty array" — keep the
  JSONPath engine authoritative.
- **Making the `doc` expression "lazy" on unused paths.** The doc is
  evaluated once per statement invocation (not per outer row; that's
  11.20d's LATERAL semantics).
- **Exposing `serde_json::Value` across the module boundary.** Keep it
  private to `json_table.rs`; surface only `Value`.
- **Duplicating the `finish_materialized_select` tail.** Refactor the
  WHERE+GROUP+ORDER+LIMIT tail of `execute_select_derived` into a
  shared helper used by both derived-subquery and JSON_TABLE paths.

## Risks

| Risk | Mitigation |
|---|---|
| `JSON_TABLE` identifier collision with user tables | Rollback-safe parser dispatch (peek-and-commit on `(`); existing table `json_table` still works. |
| `Vec<PathStep>` cloning per row | Paths live in `JsonTableSpec` by value; per-row walk borrows a reference. |
| `DEFAULT expr` correlated on outer row | Reject at compile time (scan `Expr` for `Column` references not in JSON_TABLE's own scope) → clear error. |
| NULL doc semantics divergent from PG/MariaDB | Spec locks this to "zero rows, no error"; integration test #11 asserts. |
| Re-entrancy / nested JSON_TABLE (JSON_TABLE doc is itself a JSON_TABLE) | `eval_expr_scalar` already handles arbitrary `Expr`; materialize is pure → safe. |
| Aliases clashing with a joined table | Analyzer already checks alias uniqueness in `BindContext`; no change needed. |
| Executor API churn (`from_ref: TableRef` → `&FromClause`) | One function; every caller is within `select_ctx.rs`/`select_joins_ctx.rs`. Keep the commit focused on signature change. |

---

⚡ Effort recommendation for `/implement-task`: **high**. Scope is well
bounded but touches four layers (AST, parser, analyzer, executor) and
the join-path API. Not `max`: no concurrency, no storage format, no
unsafe, no crash-recovery implications.
