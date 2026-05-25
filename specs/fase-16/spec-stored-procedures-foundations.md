# Spec: Stored procedures — foundations (CREATE/DROP/CALL + params + variables)

Phase: 16 — Server features (16.7 Stored procedures)
Task: 16.7.1 + 16.7.2 (combined first deliverable)
Status: implemented

## Context

AxiomDB currently has **no** stored-procedure support. `CALL proc(args)` and `DO expr`
parse but the executor returns `QueryResult::Empty` — a silent no-op
(`executor/exec_dispatch.rs:570`, marked `// G5.1: CALL / DO — execute as Noop`).
There is no `PROCEDURE` keyword, no `CREATE PROCEDURE`, no catalog routine object,
no procedural body language, and no local variables. db.md (lines 487-497) sets the
long-term vision: native PL/pgSQL with control flow, exceptions, cursors, IN/OUT/INOUT,
and transparent migration of existing PostgreSQL procedures.

This spec is the **first deliverable** of that vision: the foundation that makes
`CREATE PROCEDURE` / `CALL` actually execute a body of SQL statements, with
parameters and local variables, in **both** the PL/pgSQL and MySQL dialects, using a
**tree-walking interpreter**. Control flow, exceptions, cursors, and `RETURNS TABLE`
are explicitly deferred to later subphases.

## Goal

A user can `CREATE PROCEDURE` (PL/pgSQL or MySQL syntax) with `IN`/`OUT`/`INOUT`
parameters and `DECLARE`d local variables, persist it in the catalog, `CALL` it so its
body of SQL statements runs sequentially in the caller's session and transaction, read
back `OUT`/`INOUT` values, and `DROP PROCEDURE` it — and a `CALL` to a non-existent
procedure returns a real error instead of silently succeeding.

## Non-goals

Out of scope for this deliverable (deferred):

- **Control flow** (`IF`/`ELSIF`/`ELSE`, `WHILE`, `LOOP`, `FOR`, `EXIT`/`CONTINUE`) — subphase 16.7.3.
- **Exception handling** (`RAISE`, `BEGIN…EXCEPTION…END`, `DECLARE … HANDLER`, named exceptions) — Phase 16.8.
- **Cursors** (`OPEN`/`FETCH`/`CLOSE`), **`RETURNS TABLE`**, **internal transaction control** (`COMMIT`/`ROLLBACK` inside a procedure) — subphase 16.7.5.
- **Result-set-returning bare `SELECT` in a procedure body** (MySQL emits these to the client). v1 requires `SELECT … INTO var`. A bare result-set `SELECT` in a body is rejected (`NotImplemented`). — deferred.
- **MySQL `CALL p(@uservar)` write-back to session user-variables** — depends on user `@variables`, which do not exist yet (separate feature). v1 surfaces `OUT`/`INOUT` via a returned result row (see Behavior). — deferred.
- **Compiled-AST caching of procedure bodies** (PG caches; we re-parse on CALL like triggers/views). Performance optimization — deferred.
- **`ALTER PROCEDURE`**, **overloading by signature** (two procedures with the same name, different arg types) — deferred. v1 keys a procedure by `(schema, name)` only.
- `DO expr` real execution — stays a no-op for now (it is harmless; not a footgun like CALL). Tracked separately.

## Behavior

### Accepted SQL syntax

**PL/pgSQL dialect:**

```sql
CREATE [OR REPLACE] PROCEDURE [schema.]name(
    [ { IN | OUT | INOUT } arg_name data_type [, ...] ]
)
LANGUAGE plpgsql
AS $$
[ DECLARE
    var_name data_type [ := expr | DEFAULT expr ] ;
    ... ]
BEGIN
    stmt ;
    [ stmt ; ... ]
END
$$ ;
```

- Body delimited by dollar-quoting (`$$ … $$` or `$tag$ … $tag$`).
- `DECLARE` section appears **before** `BEGIN`.
- Assignment inside the body: `var := expr;` or `SELECT … INTO var [, ...] FROM …;`.

**MySQL dialect:**

```sql
CREATE PROCEDURE [schema.]name(
    [ { IN | OUT | INOUT } arg_name data_type [, ...] ]
)
BEGIN
    [ DECLARE var_name data_type [ DEFAULT expr ] ; ... ]
    stmt ;
    [ stmt ; ... ]
END
```

- Body delimited by the `BEGIN … END` block (the parser reads to the matching `END`,
  honoring nested `BEGIN … END` and `;` separators). The client-side `DELIMITER`
  directive is **not** sent over the wire; the whole `CREATE PROCEDURE … END` arrives
  as one statement.
- `DECLARE` statements appear **inside** the `BEGIN` block, before other statements.
- Assignment inside the body: `SET var = expr;` or `SELECT … INTO var [, ...] FROM …;`.

**Common to both:**

```sql
CALL [schema.]name( [ arg_expr [, ...] ] ) ;
DROP PROCEDURE [ IF EXISTS ] [schema.]name ;
```

Parameter modes, body statement set, and the interpreter are **unified** across
dialects; only the parser front-end (entry syntax, body delimiting, `DECLARE`
placement, `:=` vs `SET`) branches by dialect.

### Parameter semantics

- **IN** — bound by position from the `CALL` argument expressions, evaluated in the
  caller's context before the body runs. Read-only inside the body (assignment to an
  `IN` parameter is an error). Default mode when none is written.
- **OUT** — not bound from the caller; initialized to `NULL`; the body assigns it; its
  final value is returned to the caller (see CALL result).
- **INOUT** — bound from the `CALL` argument AND returned to the caller.

### Local variables (`DECLARE`)

- Each `DECLARE name type [init]` introduces a typed, procedure-local variable in the
  procedure's **variable frame**. Visible to all body statements after its declaration.
- Initial value: `:= expr` / `DEFAULT expr` (evaluated once at procedure entry, after
  parameters are bound); absent ⇒ `NULL`.
- Assignment: `var := expr` (PL/pgSQL), `SET var = expr` (MySQL), or `SELECT … INTO var`.
- A variable's value is coerced to its declared type on assignment (strict-mode rules of
  the session apply).

### Name resolution (variables vs columns)

- In scalar expression position inside a body statement, an **unqualified identifier**
  resolves to a procedure variable/parameter if one exists with that name.
- If the same unqualified name is **both** a procedure variable and a column of a table
  in the statement's scope, it is an **error** (`InvalidValue`, "ambiguous reference …").
  The user must qualify the column (`table.col`) or rename the variable. (PostgreSQL's
  `variable_conflict = error` behavior; a configurable mode may be added later.)

### CALL result

- If the procedure has **no** `OUT`/`INOUT` parameters: `CALL` returns `QueryResult::Empty`
  ("OK").
- If the procedure has one or more `OUT`/`INOUT` parameters: `CALL` returns a
  **one-row result set** whose columns are the `OUT`/`INOUT` parameter names (in
  declaration order) holding their final values. (PostgreSQL `CALL` semantics; dialect-neutral.)
- Affected-row counts of DML inside the body are **not** aggregated into the CALL result
  in v1.

### Execution model (tree-walking interpreter)

- `CALL` resolves the `ProcedureDef` from the catalog (via search_path for unqualified names).
- A **procedure execution context** is created: the variable frame (parameters + locals).
- The stored body text is **re-parsed** into the procedure body AST (DECLARE list +
  ordered statements), validated, then executed statement-by-statement.
- Each body statement runs in the **caller's existing session and transaction**
  (`conn_txn`), reusing the executor dispatch path. Procedure variables are made visible
  to each statement's expression evaluation via the variable frame.
- The body runs in the caller's transaction: if `CALL` is inside an explicit
  transaction, all body effects are part of it; in autocommit, the whole `CALL` commits
  as one statement. There is **no** internal transaction control in v1.
- **Statement atomicity / error propagation**: if any body statement errors, the error
  propagates out of `CALL` (no partial-statement swallowing); the session's existing
  `on_error` mode governs rollback exactly as for a top-level statement. (No
  procedure-level exception handling in v1 — that is 16.8.)
- Recursion: a procedure may `CALL` another procedure (and itself); a recursion-depth
  limit (default 256) guards against unbounded recursion (`InvalidValue` on overflow).

### Catalog object

`ProcedureDef` is a **non-table** catalog object (mirrors `HolidayCalendarDef` /
`ExchangeRateDef` — NOT embedded in `TableDef` like triggers). Keyed by `(schema, name)`.
Persisted with `to_bytes`/`from_bytes`, read via `CatalogReader::get_procedure(...)`,
written via the `CatalogWriter` (create/replace/drop).

### Public API (Rust)

```rust
// crates/axiomdb-catalog/src/schema_procedure.rs
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcParamMode { In, Out, InOut }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcParam {
    pub mode: ProcParamMode,
    pub name: String,
    pub data_type: axiomdb_types::DataType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcLanguage { PlPgSql, MySql }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcedureDef {
    pub schema_name: String,
    pub name: String,
    pub params: Vec<ProcParam>,
    pub language: ProcLanguage,
    /// Raw procedure body source (DECLARE section + BEGIN…END), re-parsed on CALL.
    pub body_sql: String,
}

impl ProcedureDef {
    pub fn to_bytes(&self) -> Vec<u8>;
    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, usize), DbError>;
}

// crates/axiomdb-catalog/src/reader.rs (CatalogReader)
pub fn get_procedure(&mut self, schema: &str, name: &str)
    -> Result<Option<ProcedureDef>, DbError>;
pub fn list_procedures(&mut self, schema: Option<&str>)
    -> Result<Vec<ProcedureDef>, DbError>;
```

```rust
// crates/axiomdb-sql/src/ast.rs
pub struct CreateProcedureStmt {
    pub or_replace: bool,
    pub name: TableRef,            // schema-qualified name reuse
    pub params: Vec<ProcParamAst>, // mode + name + type
    pub language: ProcLanguage,
    pub body_sql: String,          // captured body text
}
pub struct DropProcedureStmt { pub if_exists: bool, pub name: TableRef }
// Stmt::Call { name, args } already exists (ast.rs:1691) — gains real execution.
```

### Error cases

| Input | Expected error | Message (substring) |
|-------|----------------|---------------------|
| `CALL p()` where `p` does not exist | `DbError::ProcedureNotFound` (new) | `"procedure \"p\" does not exist"` |
| `CREATE PROCEDURE p() …` where `p` exists, no `OR REPLACE` | `DbError::ProcedureAlreadyExists` (new) | `"procedure \"p\" already exists"` |
| `DROP PROCEDURE p` where `p` does not exist (no `IF EXISTS`) | `DbError::ProcedureNotFound` | `"procedure \"p\" does not exist"` |
| `CALL p(1)` where `p` takes 2 args | `DbError::InvalidValue` | `"procedure \"p\" expects 2 argument(s), got 1"` |
| Assignment to an `IN` parameter | `DbError::InvalidValue` | `"cannot assign to IN parameter \"x\""` |
| Unqualified name is both a variable and a column | `DbError::InvalidValue` | `"ambiguous reference \"x\" (variable and column)"` |
| Body references undeclared variable in assignment target | `DbError::InvalidValue` | `"\"v\" is not a declared variable"` |
| Bare result-set `SELECT` in body (no `INTO`) | `DbError::NotImplemented` | `"result-set SELECT in procedure body (use SELECT … INTO); deferred"` |
| Control-flow / `RAISE` / cursor / `RETURNS TABLE` in body | `DbError::NotImplemented` | `"… in procedures not yet supported (Phase 16.7.x/16.8)"` |
| `CALL` recursion exceeds depth limit | `DbError::InvalidValue` | `"procedure recursion depth limit (256) exceeded"` |
| Syntax error in body at CREATE time | `DbError::ParseError` | (parser message + position) |
| Type coercion failure on parameter/variable assignment | `DbError::TypeMismatch` | (expected/got) |

New `DbError` variants to add: `ProcedureNotFound { name: String }` (SQLSTATE `42883`),
`ProcedureAlreadyExists { schema: String, name: String }` (SQLSTATE `42723`).

## Edge cases

- [ ] Empty body `BEGIN END` / `$$ BEGIN END $$` — valid, runs nothing, returns Empty (or OUT row if OUT params).
- [ ] Procedure with zero parameters; with only IN; with only OUT; with INOUT mix.
- [ ] `OR REPLACE` over an existing procedure (replaces definition atomically).
- [ ] `DROP PROCEDURE IF EXISTS` on a missing procedure — succeeds (no error).
- [ ] Unqualified `CALL` resolves through `search_path`; qualified `schema.proc` resolves directly; missing schema → error.
- [ ] Procedure body contains internal `;`-separated statements (parser must not stop at the first `;`).
- [ ] MySQL dialect: nested `BEGIN … END` inside the body parses to the matching `END`.
- [ ] PL/pgSQL dialect: dollar-quote with a tag `$tag$ … $tag$`; body text containing `$$`.
- [ ] Unicode / non-ASCII in identifiers, string literals, and the body text (round-trips through `to_bytes`/`from_bytes`).
- [ ] `NULL` argument passed to an IN parameter; OUT parameter never assigned (returns `NULL`).
- [ ] Variable shadows nothing vs shadows a column (ambiguity → error).
- [ ] `SELECT … INTO var` returning zero rows (var set to `NULL`), exactly one row (assigned), more than one row (error — `"query returned more than one row"`).
- [ ] `CALL` inside an explicit transaction: body effects roll back with the outer `ROLLBACK`.
- [ ] Error mid-body: subsequent statements do not run; error surfaces from `CALL`; session `on_error` governs rollback.
- [ ] Procedure persists across catalog reopen (serialization round-trip).
- [ ] Recursive `CALL` (self and mutual) within and beyond the depth limit.
- [ ] Catalog `from_bytes` on truncated/garbage bytes → `ParseError` (no panic).

## On-disk format

`ProcedureDef` binary layout (little-endian; mirrors `HolidayCalendarDef`):

```
[schema_len: u8][schema: UTF-8]
[name_len:   u8][name:   UTF-8]
[language:   u8]                       ; 0 = PlPgSql, 1 = MySql
[param_count: u16]
  repeated param_count times:
    [mode: u8]                         ; 0=IN, 1=OUT, 2=INOUT
    [pname_len: u8][pname: UTF-8]
    [type_tag: u8][type_payload: …]    ; reuse the existing DataType codec
[body_len: u32][body: UTF-8]
```

Compatibility rule: a leading `version: u8` byte MAY be prefixed in a later subphase to
evolve the format; v1 is implicitly version 0. `from_bytes` returns `(Self, consumed)`
and validates every length prefix (truncation ⇒ `ParseError`, never panic).

## Performance budget

Not a hot path. Targets (informational, not gating):

| Operation | Target |
|-----------|--------|
| `CALL` of a small body (≤5 statements) | within ~1.5× of issuing the same statements directly (re-parse + frame overhead) |
| `CREATE PROCEDURE` | one catalog write, comparable to `CREATE TRIGGER` |

Body re-parse-per-CALL is acceptable for v1 (matches triggers/views). Compiled-AST
caching is a documented future optimization.

## Dependencies

- Depends on: existing parser/analyzer/executor (`dispatch_ctx`, `execute_with_ctx`),
  the non-table catalog object pattern (`schema_holiday_calendar.rs`,
  `CatalogReader`/`CatalogWriter`), `DataType` codec, the trigger
  re-parse-on-execute pattern (`executor/trigger.rs`).
- Blocks: 16.7.3 (control flow), 16.8 (exceptions), 16.7.5 (cursors / RETURNS TABLE /
  internal txn) — all build on this procedure execution context + interpreter.

## Resolved decisions (confirm at approval)

These were decided during brainstorm/spec to keep the spec a contract; flagged for the
user to sign off:

1. **OUT/INOUT surfacing = a returned one-row result set** (PostgreSQL-style),
   dialect-neutral. MySQL `CALL p(@uservar)` write-back to user `@variables` is deferred
   (user `@variables` do not exist yet).
2. **Bare result-set `SELECT` in a body is rejected** in v1 (`NotImplemented`); use
   `SELECT … INTO var`. Avoids multi-result-set wire complexity.
3. **Variable/column name ambiguity is an error** (no silent precedence).
4. **A procedure is keyed by `(schema, name)`** — no overloading by signature in v1.
5. **Body stored as text, re-parsed on CALL** (validated at CREATE), like triggers/views;
   no compiled-AST cache yet.

## Done criteria

- [ ] Public API matches the signatures above (`ProcedureDef`, AST, `CatalogReader::get_procedure`).
- [ ] `PROCEDURE` keyword in the lexer; `CREATE [OR REPLACE] PROCEDURE` and `DROP PROCEDURE [IF EXISTS]` parse in **both** dialects.
- [ ] Procedure body parses (DECLARE + BEGIN…END, internal `;`, nested BEGIN…END for MySQL, dollar-quoting for PL/pgSQL) and stores as text.
- [ ] `ProcedureDef` persists + round-trips (`to_bytes`/`from_bytes`) and survives catalog reopen.
- [ ] `CALL` executes the body sequentially in the caller's session/transaction; IN params bind; OUT/INOUT surface as a result row; DECLARE locals + `:=`/`SET`/`SELECT…INTO` work.
- [ ] **Safety fix**: `CALL` to an unknown procedure returns `ProcedureNotFound` (the `exec_dispatch.rs:570` silent-noop for `Stmt::Call` is removed; `DO` may remain a noop).
- [ ] `information_schema.routines` lists created procedures (routine_name, routine_schema, routine_type='PROCEDURE', data_type/params best-effort).
- [ ] Every edge case above has a test (unit + integration with `MemoryStorage` and a reopen test for persistence).
- [ ] `cargo nextest run -p axiomdb-catalog -p axiomdb-sql` passes (Lima VM).
- [ ] `cargo clippy --workspace -- -D warnings` clean; `cargo fmt --check` clean.
- [ ] Wire smoke test (`tools/wire-test.py`): `CREATE PROCEDURE` + `CALL` over the MySQL wire protocol returns the expected result/OUT row; unknown `CALL` returns an error.
- [ ] Rustdoc on every new public item.
- [ ] docs-site updated: `sql-reference/ddl.md` (CREATE/DROP PROCEDURE), `sql-reference/dml.md` (CALL), `internals/sql-parser.md` + a new `internals/stored-procedures.md`; `development/roadmap.md` marks 16.7.1/.2 progress.

## References

- db.md lines 487-497 (stored-procedure vision).
- Brainstorm decisions: both dialects, tree-walker, foundations+params first.
- Catalog precedent: `crates/axiomdb-catalog/src/schema_holiday_calendar.rs`.
- Trigger body execution: `crates/axiomdb-sql/src/executor/trigger.rs` (`run_one_statement_trigger`).
- PostgreSQL PL/pgSQL: `research/postgres/src/pl/plpgsql/src/pl_exec.c` (tree-walker, `exec_assign_value`, datum array), `src/include/catalog/pg_proc.h` (`prosrc`).
- MariaDB: `research/mariadb-server/sql/sp_head.{h,cc}`, `sp_rcontext` (param modes, variable frame).
