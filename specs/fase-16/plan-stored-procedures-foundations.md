# Plan: Stored procedures — foundations (CREATE/DROP/CALL + params + variables)

Phase: 16 — Server features (16.7 Stored procedures)
Task: 16.7.1 + 16.7.2 (combined first deliverable)
Spec: specs/fase-16/spec-stored-procedures-foundations.md
Status: in-progress

## Summary

Build stored procedures bottom-up so every commit compiles and tests green: first the
leaf pieces with no dependents (error variants, the `ProcedureDef` catalog object +
serialization, then its persistence), then the front-end (lexer keyword + dollar-quoting,
AST, parser for both dialects, body sub-parser), then the analyzer and the DDL executor
(CREATE/DROP + the CALL safety fix), and finally the runtime core (variable frame +
tree-walking interpreter) wired into `CALL`, plus `information_schema.routines` and the
closing protocol. Bottom-up ordering means the hardest piece (the interpreter, Step 10)
lands on top of already-tested catalog/parser/analyzer layers, and the safety fix
(Step 9) ships early so `CALL` stops silently succeeding even before the full interpreter
is done.

## Dependencies

Must be done first:
- [x] spec-stored-procedures-foundations approved

Blocks (until this plan is done):
- [ ] 16.7.3 control flow (IF/LOOP/WHILE)
- [ ] 16.8 exception handling
- [ ] 16.7.5 cursors / RETURNS TABLE / internal txn

## Affected files

New files:
- `crates/axiomdb-catalog/src/schema_procedure.rs` — `ProcedureDef`, `ProcParam`, `ProcParamMode`, `ProcLanguage` + `to_bytes`/`from_bytes` + unit tests.
- `crates/axiomdb-sql/src/executor/procedure.rs` — procedure execution context (variable frame) + tree-walking interpreter.
- `crates/axiomdb-sql/tests/integration_stored_procedures.rs` — end-to-end integration tests.
- `crates/axiomdb-catalog/tests/integration_procedure_catalog.rs` — catalog persistence + reopen tests.
- `docs-site/docs/internals/stored-procedures.md` — technical doc.

Modified files:
- `crates/axiomdb-core/src/error.rs` — `ProcedureNotFound`, `ProcedureAlreadyExists` + SQLSTATE.
- `crates/axiomdb-catalog/src/{lib.rs, reader.rs, writer.rs, bootstrap.rs, schema.rs, page_ids}` — `get_procedure`/`list_procedures`, `upsert_procedure`/`delete_procedure`, `ensure_procedures_root`, WAL table_id const, re-exports.
- `crates/axiomdb-sql/src/lexer.rs` — `PROCEDURE` keyword; dollar-quoted string token.
- `crates/axiomdb-sql/src/ast.rs` — `CreateProcedureStmt`, `DropProcedureStmt`, `ProcParamAst`, `ProcLanguage`, `ProcBody` + `Stmt` variants.
- `crates/axiomdb-sql/src/parser/{mod.rs, ddl.rs}` — CREATE/DROP PROCEDURE dispatch + parsers + body capture + body sub-parser.
- `crates/axiomdb-sql/src/{analyzer_ddl.rs, analyzer_stmt.rs}` — analyze the new statements.
- `crates/axiomdb-sql/src/executor/{exec_dispatch.rs, mod.rs}` — CREATE/DROP PROCEDURE + CALL real execution (remove noop at exec_dispatch.rs:570).
- `crates/axiomdb-sql/src/session.rs` — recursion-depth counter (procedure call depth).
- `crates/axiomdb-sql/src/information_schema.rs` — `routines` view.
- `crates/axiomdb-sql/src/plan_deps.rs` — deps for the new statements (DDL: invalidate; CALL: none/dynamic).
- `docs-site/docs/sql-reference/{ddl.md, dml.md}`, `development/roadmap.md`; `docs/progreso.md`; `memory/{project_state.md, architecture.md}`.

---

## Step 1 — DbError variants for procedures

**Goal:** add the two error variants the whole feature reports.
**Files:** `crates/axiomdb-core/src/error.rs`.
**Approach:** TDD — assert the SQLSTATE mapping first.

### Test to add
```rust
// crates/axiomdb-core/src/error.rs (#[cfg(test)])
#[test]
fn procedure_error_sqlstates() {
    assert_eq!(DbError::ProcedureNotFound { name: "p".into() }.sqlstate(), "42883");
    assert_eq!(
        DbError::ProcedureAlreadyExists { schema: "public".into(), name: "p".into() }.sqlstate(),
        "42723"
    );
}
```

### Implementation outline
```rust
ProcedureNotFound { name: String },                       // near TableNotFound (:48)
ProcedureAlreadyExists { schema: String, name: String },  // near TableAlreadyExists (:264)
// Display impls + sqlstate() arms (~:389): 42883 / 42723.
```

### Verification
```bash
./tools/vm.sh test -p axiomdb-core error
```

### Commit
`feat(fase-16): ProcedureNotFound / ProcedureAlreadyExists error variants`

---

## Step 2 — Catalog `ProcedureDef` + serialization

**Goal:** the catalog data type + byte codec (no persistence yet).
**Files:** `crates/axiomdb-catalog/src/schema_procedure.rs` (new), `lib.rs` (re-export), `schema.rs` (mod).
**Approach:** TDD — round-trip tests first (mirror `schema_holiday_calendar.rs` tests).

### Test to add
```rust
#[test]
fn roundtrip_proc_with_all_param_modes() {
    let def = ProcedureDef {
        schema_name: "public".into(), name: "p".into(),
        params: vec![
            ProcParam { mode: ProcParamMode::In,    name: "a".into(), data_type: DataType::Int },
            ProcParam { mode: ProcParamMode::Out,   name: "b".into(), data_type: DataType::Text },
            ProcParam { mode: ProcParamMode::InOut, name: "c".into(), data_type: DataType::Bool },
        ],
        language: ProcLanguage::PlPgSql,
        body_sql: "BEGIN b := 'x'; END".into(),
    };
    let bytes = def.to_bytes();
    let (decoded, used) = ProcedureDef::from_bytes(&bytes).unwrap();
    assert_eq!(decoded, def);
    assert_eq!(used, bytes.len());
}

#[test]
fn from_bytes_truncated_is_error_not_panic() { /* truncate every prefix boundary */ }

#[test]
fn roundtrip_unicode_body_and_names() { /* non-ASCII identifiers + body */ }
```

### Implementation outline
Per the spec "On-disk format". Reuse the existing `DataType` codec for `type_tag`/payload.
`to_bytes`/`from_bytes` validate every length prefix; never panic.

### Verification
```bash
./tools/vm.sh test -p axiomdb-catalog procedure
```

### Commit
`feat(fase-16): ProcedureDef catalog type + byte codec`

---

## Step 3 — Catalog persistence (root + upsert/delete/get/list)

**Goal:** persist procedures in a dedicated catalog tree; survive reopen.
**Files:** `crates/axiomdb-catalog/src/{bootstrap.rs, writer.rs, reader.rs, lib.rs}`, page-ids struct; `crates/axiomdb-catalog/tests/integration_procedure_catalog.rs` (new).
**Approach:** TDD — create→get, reopen→get, delete, upsert-replace. Mirror `axiom_holiday_calendars` (`ensure_holiday_calendars_root`, `upsert_holiday_calendar`:2100, `delete_holiday_calendar`:2143, `get_holiday_calendar`:914) and add a WAL table_id const (writer.rs:93 pattern).

### Test to add
```rust
#[test]
fn procedure_persists_across_reopen() {
    // create catalog, upsert_procedure(p), reopen catalog, get_procedure("public","p") == Some(p)
}
#[test]
fn upsert_replaces_existing() { /* same (schema,name) replaces */ }
#[test]
fn delete_returns_false_when_absent() {}
#[test]
fn list_procedures_filters_by_schema() {}
```

### Implementation outline
```rust
// reader.rs
pub fn get_procedure(&mut self, schema: &str, name: &str) -> Result<Option<ProcedureDef>, DbError>;
pub fn list_procedures(&mut self, schema: Option<&str>) -> Result<Vec<ProcedureDef>, DbError>;
// writer.rs
pub fn upsert_procedure(&mut self, def: ProcedureDef) -> Result<(), DbError>;
pub fn delete_procedure(&mut self, schema: &str, name: &str) -> Result<bool, DbError>;
// bootstrap.rs
pub fn ensure_procedures_root(storage) -> Result<u64, DbError>;
// key = "schema\0name" bytes; value = ProcedureDef::to_bytes()
```

### Verification
```bash
./tools/vm.sh test -p axiomdb-catalog --test integration_procedure_catalog
```

### Commit
`feat(fase-16): persist procedures in axiom_procedures catalog tree`

---

## Step 4 — Lexer: `PROCEDURE` keyword + dollar-quoted strings

**Goal:** tokenize `PROCEDURE` and PL/pgSQL `$$ … $$` / `$tag$ … $tag$` bodies.
**Files:** `crates/axiomdb-sql/src/lexer.rs`.
**Approach:** TDD — lex tests first. Dollar-quoting is a logos callback (the default `$` path errors at lexer.rs:1244).

### Test to add
```rust
#[test]
fn lex_procedure_keyword() { assert_eq!(lex_one("PROCEDURE"), Token::Procedure); }
#[test]
fn lex_dollar_quoted_simple() {
    assert_eq!(lex_one("$$ a; b; $$"), Token::DollarString(" a; b; ".into()));
}
#[test]
fn lex_dollar_quoted_tagged() {
    assert_eq!(lex_one("$body$ x $$ y $body$"), Token::DollarString(" x $$ y ".into()));
}
#[test]
fn lex_lone_dollar_still_errors_or_param() { /* preserve existing $ / $1 behavior */ }
```

### Implementation outline
- `#[token("PROCEDURE", ignore(ascii_case))] Procedure,`
- Dollar-quote: a logos callback that, on `$`, tries to read an optional tag up to the next `$`, then scans to the matching `$tag$`; emits `Token::DollarString(inner)`. Must NOT clash with positional params (`$1`) if those exist — guard: a tag is `[A-Za-z_][A-Za-z0-9_]*` or empty followed by `$`.

### Verification
```bash
./tools/vm.sh test -p axiomdb-sql lexer
```

### Commit
`feat(fase-16): lexer PROCEDURE keyword + dollar-quoted strings`

---

## Step 5 — AST nodes

**Goal:** typed AST for CREATE/DROP PROCEDURE + procedure body.
**Files:** `crates/axiomdb-sql/src/ast.rs`.
**Approach:** types + a parse smoke test deferred to Step 6 (AST alone has no behavior; merge-test with parser).

### Implementation outline
```rust
pub enum ProcLanguage { PlPgSql, MySql }
pub enum ProcParamMode { In, Out, InOut }
pub struct ProcParamAst { pub mode: ProcParamMode, pub name: String, pub data_type: DataType }
pub struct ProcVarDecl { pub name: String, pub data_type: DataType, pub init: Option<Expr> }
pub struct ProcBody { pub declares: Vec<ProcVarDecl>, pub statements: Vec<Stmt> }
pub struct CreateProcedureStmt {
    pub or_replace: bool, pub name: TableRef,
    pub params: Vec<ProcParamAst>, pub language: ProcLanguage,
    pub body_sql: String,     // raw, stored in catalog
}
pub struct DropProcedureStmt { pub if_exists: bool, pub name: TableRef }
// Stmt::CreateProcedure(CreateProcedureStmt), Stmt::DropProcedure(DropProcedureStmt)
// Stmt::Call already exists.
// Body-only statements (parsed from body, not top-level): assignment + SELECT INTO.
pub enum ProcStmt {            // what the body sub-parser produces
    Sql(Stmt),                 // INSERT/UPDATE/DELETE/SELECT…INTO
    Assign { target: String, value: Expr },   // v := expr  /  SET v = expr
}
```

### Verification (compile only here)
```bash
./tools/vm.sh test -p axiomdb-sql --no-run
```

### Commit
`feat(fase-16): AST for CREATE/DROP PROCEDURE + procedure body`

---

## Step 6 — Parser: CREATE/DROP PROCEDURE (both dialects) + body capture

**Goal:** parse both dialects; capture body as raw text; validate-parse the body.
**Files:** `crates/axiomdb-sql/src/parser/{mod.rs (dispatch :972 / :1102), ddl.rs}`.
**Approach:** TDD — parse tests for both dialects + errors.

### Test to add
```rust
#[test] fn parse_pg_procedure_with_params() {
    let s = parse("CREATE PROCEDURE p(IN a INT, OUT b TEXT) LANGUAGE plpgsql AS $$ BEGIN b := 'x'; END $$");
    // assert name, params modes/types, language=PlPgSql, body captured
}
#[test] fn parse_mysql_procedure_begin_end() {
    let s = parse("CREATE PROCEDURE p(IN a INT) BEGIN INSERT INTO t VALUES (a); END");
    // language=MySql, body captured to matching END
}
#[test] fn parse_mysql_nested_begin_end() { /* BEGIN … BEGIN … END … END to outer END */ }
#[test] fn parse_or_replace() {}
#[test] fn parse_drop_procedure_if_exists() {}
#[test] fn parse_body_with_internal_semicolons_not_truncated() {}
```

### Implementation outline
- `parse_create` (mod.rs:972): add `Token::Procedure` arm → `ddl::parse_create_procedure(self, or_replace)`; thread the existing OR REPLACE handling (:1015).
- `parse_drop` dispatch (~:1102): add `Token::Procedure` → `ddl::parse_drop_procedure`.
- `parse_create_procedure`: name (qualified), param list `( {IN|OUT|INOUT}? name type, … )`, then dialect detection:
  - `LANGUAGE plpgsql AS $tag$ … $tag$` → `ProcLanguage::PlPgSql`, body = the `DollarString` inner text.
  - `BEGIN` directly → `ProcLanguage::MySql`, body = raw slice from `BEGIN` to the matching `END` using a depth counter over `Token::Begin`/`Token::End` + `slice_sql()` (mod.rs:173).
- Validate-parse the captured body via the Step-7 sub-parser; surface `ParseError` with position.

### Verification
```bash
./tools/vm.sh test -p axiomdb-sql --test integration_stored_procedures parse
./tools/vm.sh test -p axiomdb-sql parser
```

### Commit
`feat(fase-16): parse CREATE/DROP PROCEDURE (PL/pgSQL + MySQL)`

---

## Step 7 — Procedure body sub-parser (DECLARE list + statements)

**Goal:** turn body text into `ProcBody` (declares + ordered `ProcStmt`s).
**Files:** `crates/axiomdb-sql/src/parser/ddl.rs` (or `parser/proc_body.rs`).
**Approach:** TDD — body parse tests; reject deferred constructs with `NotImplemented`.

### Test to add
```rust
#[test] fn body_parses_declares_then_statements() {
    let b = parse_proc_body("DECLARE v INT := 1; BEGIN SET v = v + 1; INSERT INTO t VALUES (v); END", MySql);
    // 1 declare, 2 statements (Assign, Sql)
}
#[test] fn body_select_into_is_assignment_form() {}
#[test] fn body_bare_select_is_not_implemented() {
    assert!(matches!(parse_proc_body("BEGIN SELECT 1; END", MySql), Err(DbError::NotImplemented{..})));
}
#[test] fn body_if_loop_while_is_not_implemented() {}
```

### Implementation outline
- PL/pgSQL: `DECLARE` section before `BEGIN`; MySQL: `DECLARE` lines first inside `BEGIN`.
- Each statement: detect `var := expr` / `SET var = expr` → `ProcStmt::Assign`; `SELECT … INTO …` → assignment form; `INSERT/UPDATE/DELETE` → `ProcStmt::Sql`.
- Reject (NotImplemented, exact messages from spec): bare result-set `SELECT`, `IF/LOOP/WHILE/FOR`, `RAISE`, cursor ops, `RETURN`/`RETURNS TABLE`, `COMMIT/ROLLBACK`.

### Verification
```bash
./tools/vm.sh test -p axiomdb-sql --test integration_stored_procedures body
```

### Commit
`feat(fase-16): procedure body sub-parser (DECLARE + statements)`

---

## Step 8 — Analyzer for CREATE/DROP PROCEDURE

**Goal:** analyze the new top-level statements (resolve schema, validate param types).
**Files:** `crates/axiomdb-sql/src/{analyzer_ddl.rs, analyzer_stmt.rs}`, `plan_deps.rs`.
**Approach:** TDD — analysis accepts valid, rejects duplicate param names / unknown types.

### Test / impl
- Resolve target schema via search_path/default; validate param names unique; validate types; pass body text through unchanged (body analyzed lazily at CALL, like triggers).
- `plan_deps`: CREATE/DROP PROCEDURE → DDL (invalidate_all). CALL → dynamic (no static deps; like the existing `Stmt::Call` arm at plan_deps.rs:253).

### Verification
```bash
./tools/vm.sh test -p axiomdb-sql analyze_procedure
```

### Commit
`feat(fase-16): analyze CREATE/DROP PROCEDURE`

---

## Step 9 — Executor: CREATE/DROP PROCEDURE + CALL safety fix

**Goal:** persist on CREATE/DROP; make `CALL`-unknown error (remove the silent noop) — value lands even before the interpreter.
**Files:** `crates/axiomdb-sql/src/executor/exec_dispatch.rs` (:570), `executor/mod.rs`.
**Approach:** TDD — CREATE then catalog has it; DROP removes; duplicate errors; **CALL unknown → ProcedureNotFound**.

### Test to add
```rust
#[test] fn create_procedure_persists_and_drop_removes() {}
#[test] fn create_duplicate_without_or_replace_errors() {}
#[test] fn call_unknown_procedure_errors_not_silent() {
    let e = run("CALL nope()");
    assert!(matches!(e, Err(DbError::ProcedureNotFound{..})));   // was Ok(Empty) before
}
```

### Implementation outline
- `Stmt::CreateProcedure` → `CatalogWriter::upsert_procedure` (respect OR REPLACE / AlreadyExists); `invalidate_all`.
- `Stmt::DropProcedure` → `delete_procedure` (respect IF EXISTS / NotFound).
- Replace `Stmt::Call { .. } | Stmt::Do { .. } => Ok(QueryResult::Empty)` (line 570): keep `Do` as noop; route `Call` → `procedure::execute_call(...)` (Step 11). Until Step 11 lands, `Call` resolves the proc and returns `NotImplemented` for a found-but-not-yet-runnable body — but since Steps 10/11 are in the same deliverable, wire straight to the interpreter.

### Verification
```bash
./tools/vm.sh test -p axiomdb-sql --test integration_stored_procedures ddl_exec
```

### Commit
`feat(fase-16): execute CREATE/DROP PROCEDURE + CALL unknown→error (safety fix)`

---

## Step 10 — Procedure execution context + tree-walking interpreter (core, max effort)

**Goal:** run a procedure body sequentially with a variable frame; bind IN; assign vars; SELECT INTO; collect OUT/INOUT.
**Files:** `crates/axiomdb-sql/src/executor/procedure.rs` (new), `session.rs` (depth counter).
**Approach:** TDD — exhaustive behavior tests (this is the riskiest step; prototype the variable-resolution mechanism FIRST).

### Test to add (representative; all spec edge cases covered)
```rust
#[test] fn proc_runs_sequential_dml_with_in_param() {}      // CALL p(5) inserts using a
#[test] fn proc_declare_assign_and_use_variable() {}         // DECLARE v; v := 2; INSERT v
#[test] fn proc_select_into_variable() {}                    // SELECT count(*) INTO v
#[test] fn proc_out_param_returned_as_row() {}               // CALL → 1-row result of OUT
#[test] fn proc_inout_bound_and_returned() {}
#[test] fn proc_assign_to_in_param_errors() {}
#[test] fn proc_variable_column_ambiguity_errors() {}
#[test] fn proc_select_into_zero_one_many_rows() {}          // NULL / value / error
#[test] fn proc_error_midbody_propagates_and_rolls_back() {} // in explicit txn
#[test] fn proc_empty_body_ok() {}
```

### Implementation outline
```rust
pub struct ProcFrame { vars: IndexMap<String, (DataType, ProcParamMode_or_local, Value)> }

pub fn execute_call(def: &ProcedureDef, args: &[Expr], exec_ctx, ctx) -> Result<QueryResult, DbError> {
    // 1. depth guard (ctx.proc_depth += 1; <= 256 else error)
    // 2. arity check; bind IN/INOUT args (eval in caller ctx) into frame; OUT=NULL
    // 3. re-parse body (parse_with_sql_mode honoring def.language) → ProcBody (cache later)
    // 4. init DECLARE vars (eval init exprs against frame)
    // 5. for each ProcStmt:
    //      Assign{target,value} -> eval `value` (with frame substitution) -> coerce -> store
    //      Sql(stmt) -> substitute frame variables -> dispatch_ctx via conn_txn take()/Some()
    //                   (SELECT…INTO: run, enforce ≤1 row, assign targets)
    // 6. build OUT/INOUT result row (or Empty)
}
```
**Variable resolution mechanism (the key risk — decide + prototype first):** before
dispatching a body `Sql` statement, rewrite unqualified `Expr::Identifier(name)` where
`name` is a frame variable into a bound value (reuse the parameter-binding path so types
are preserved — NOT text substitution). Qualified `table.col` is never a variable.
Ambiguity: if an unqualified name matches both a frame variable and a resolvable column
of a table in the statement, return `InvalidValue` ("ambiguous reference"). Validate this
approach on `proc_variable_column_ambiguity_errors` + `proc_declare_assign_and_use_variable`
before building the rest. If it proves infeasible, STOP and revise the plan (variable
precedence vs evaluator extension).

### Verification
```bash
./tools/vm.sh test -p axiomdb-sql --test integration_stored_procedures
```

### Commit
`feat(fase-16): tree-walking procedure interpreter + variable frame`

---

## Step 11 — Wire CALL → interpreter + error/recursion cases

**Goal:** `CALL` end-to-end with all error cases from the spec.
**Files:** `crates/axiomdb-sql/src/executor/{exec_dispatch.rs, procedure.rs}`, `session.rs`.
**Approach:** TDD — arity, recursion limit, autocommit + explicit-txn, qualified/unqualified resolution.

### Test to add
```rust
#[test] fn call_resolves_through_search_path() {}
#[test] fn call_arity_mismatch_errors() {}
#[test] fn call_recursion_depth_limit() {}
#[test] fn call_inside_explicit_txn_rolls_back_with_outer() {}
```

### Commit
`feat(fase-16): wire CALL to the interpreter + error/recursion handling`

---

## Step 12 — information_schema.routines

**Goal:** list procedures in `information_schema.routines`.
**Files:** `crates/axiomdb-sql/src/information_schema.rs`.
**Approach:** TDD — after CREATE, the view lists it (routine_name/schema, routine_type='PROCEDURE').

### Commit
`feat(fase-16): information_schema.routines for procedures`

---

## Step 13 — Wire together / closing

**Goal:** full workspace green + wire test + docs + memory + progreso.

### Verification against spec (walk every Done criterion)
```bash
./tools/vm.sh test            # workspace nextest — clean
./tools/vm.sh clippy          # -D warnings — clean
rustfmt --edition 2021 <only my files>   # fmt
# wire smoke test: overwrite tools/wire-test.py with CREATE PROCEDURE + CALL (OUT row) +
# unknown-CALL-errors assertions; pkill axiomdb-server + rebuild release + run.
./tools/vm.sh wire
```
- docs-site: `sql-reference/ddl.md` (CREATE/DROP PROCEDURE), `sql-reference/dml.md` (CALL), `internals/sql-parser.md`, new `internals/stored-procedures.md`; `development/roadmap.md`.
- `docs/progreso.md`: mark 16.7.1/16.7.2 progress; `memory/{project_state.md, architecture.md}`; `memory/lessons.md` if surprising.
- Add `callout-advantage`/`callout-design` where relevant (e.g., both-dialect support).

### Final commit
`feat(fase-16): stored-procedures foundations (CREATE/DROP/CALL + params + vars)`

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| Procedure-body multi-statement parser (both dialects: MySQL BEGIN..END matching + nesting, PG dollar-quoting) | high | Lexer dollar-quote (Step 4) + depth-counted BEGIN/END slice (Step 6); dedicated parser tests incl. nesting + internal `;` |
| Variable-vs-column resolution in body expression eval | high | Step 10 prototypes the parameter-binding rewrite FIRST; ambiguity = error; if infeasible, STOP + revise plan (precedence vs evaluator extension) |
| Statement-atomicity / error propagation through body within caller's txn | medium | Reuse the trigger conn_txn take()/Some() pattern + existing on_error; explicit-txn rollback test (Step 10/11) |
| Dollar-quote lexer change breaks existing `$`/param lexing | medium | Guard tag grammar; regression-run full lexer + parser suites in Step 4 |
| Catalog format evolution | low | length-prefixed codec + truncation tests (Step 2); reserve a version byte |
| `DataType` codec reuse for params | low | reuse existing serializer; roundtrip test (Step 2) |

## Rollback plan

1. Each step is an isolated commit; `git revert <step>` peels back cleanly.
2. The catalog tree (`axiom_procedures`) is additive — absent on older DBs, `ensure_*_root` creates lazily; no migration needed.
3. If abandoned mid-way: the CALL safety fix (Step 9) is independently valuable and can stay; revert Steps 10-12 leaving CALL→NotImplemented for found procedures.

## Estimated effort

Total: ~3-5 focused days (max-effort core in Step 10).
Per step (rough): S1 20m · S2 1h · S3 2h · S4 2h · S5 30m · S6 3h · S7 2h · S8 1h · S9 1.5h · **S10 1-1.5d** · S11 2h · S12 1h · S13 (closing) 3h.
