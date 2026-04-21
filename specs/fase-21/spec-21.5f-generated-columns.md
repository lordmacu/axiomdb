# Spec: 21.5f — Generated Columns

Phase: 21 — Advanced SQL
Task: 21.5f GENERATED ALWAYS AS columns
Status: approved

## Context

Phase 21.5 closed PostgreSQL `ON CONFLICT` and SQL-standard heap `MERGE`.
The next compatibility gap is `GENERATED ALWAYS AS (expr)` columns, called
out in `docs/progreso.md` and `docs/gaps-mysql-compat.md`. The current parser
does not attach generated-column metadata to `ColumnDef`, and the executor
does not compute generated values on writes.

## Goal

Implement `GENERATED ALWAYS AS (expr) STORED` columns for `CREATE TABLE`,
persist their metadata in the catalog, and materialize values on INSERT and
UPDATE paths.

## Non-goals

- True `VIRTUAL` generated columns. The parser/catalog shape may reserve the
  kind, but `CREATE TABLE ... VIRTUAL` must return a clear `NotImplemented`.
- `ALTER TABLE ADD/MODIFY ... GENERATED`. Backfilling existing rows and
  rewriting physical layouts is deferred.
- Generated-column dependency chains. In this subphase, a generated expression
  may reference only non-generated columns in the same table.
- Volatile functions, subqueries, aggregates, window functions, or table-valued
  constructs inside generated expressions.
- New optimizer support. STORED generated columns are ordinary physical values
  after write-time materialization.

## Behavior

### Public API

The SQL AST and catalog gain explicit generated-column metadata:

```rust
pub enum GeneratedColumnKind {
    Stored,
    Virtual,
}

pub struct GeneratedColumn {
    pub expr: Expr,
    pub kind: GeneratedColumnKind,
}

pub struct ColumnDef {
    pub generated: Option<GeneratedColumn>,
    // existing fields unchanged
}

pub struct CatalogColumnDef {
    pub generated_expr: Option<String>,
    pub generated_stored: bool,
    // existing fields unchanged
}
```

Exact names may differ to match local style, but generated expression text and
kind must be persisted and round-trip through the catalog.

### Grammar

Supported successful form:

```sql
CREATE TABLE orders (
    price INT,
    qty INT,
    total INT GENERATED ALWAYS AS (price * qty) STORED
);
```

Parsed but rejected at DDL execution:

```sql
CREATE TABLE t (
    a INT,
    b INT GENERATED ALWAYS AS (a + 1) VIRTUAL
);
```

`GENERATED ALWAYS` is required when the generated-clause form is used. The
expression is enclosed in parentheses and the kind must be `STORED` for this
subphase.

### CREATE TABLE semantics

- A STORED generated column is catalog-visible and has a normal physical column
  slot in row values.
- The generated expression is serialized as re-parseable SQL text.
- The expression must reference only existing non-generated columns from the
  same table.
- The expression must not reference itself, another generated column, unknown
  columns, subqueries, aggregates, or window functions.
- A generated column cannot also declare `DEFAULT`, `ON UPDATE`, or
  `AUTO_INCREMENT`.
- `VIRTUAL` returns `DbError::NotImplemented` with a message that mentions
  virtual generated columns.

### INSERT semantics

- For every row, defaults and auto-increment are resolved first.
- STORED generated columns are then recomputed from the row's non-generated
  values and overwrite any placeholder value.
- Explicit non-`DEFAULT` values for generated columns are rejected.
- Explicit `DEFAULT` for a generated column is accepted and means "compute the
  generated value".
- INSERT without a column list may omit trailing generated columns. Missing
  generated columns are computed.
- CHECK constraints, FK checks, UNIQUE indexes, partial indexes, GIN/index
  maintenance, and RETURNING observe the computed stored value.

### UPDATE semantics

- Assigning a non-`DEFAULT` value directly to a generated column is rejected.
- After normal assignments and `ON UPDATE` expressions are applied, all STORED
  generated columns are recomputed.
- Recomputing all STORED generated columns on every UPDATE is acceptable; no
  dependency-based optimization is required.
- UPDATE constraints, FK checks, indexes, and RETURNING observe the recomputed
  stored value.

### MERGE / UPSERT / REPLACE semantics

- `INSERT ... ON CONFLICT`, MySQL ODKU, `REPLACE INTO`, and MERGE insert arms
  use the same INSERT materialization rule.
- Conflict-update and MERGE update arms use the same UPDATE recomputation rule.
- `EXCLUDED.generated_col` and `VALUES(generated_col)` see the proposed row
  after generated values have been computed.

## Error cases

| Input | Expected error | Message requirement |
|---|---|---|
| `... GENERATED ALWAYS AS (a + 1) VIRTUAL` | `DbError::NotImplemented` | Mentions virtual generated columns |
| Generated expression references unknown column | `DbError::ColumnNotFound` or semantic error | Missing column name |
| Generated expression references itself | semantic error | Mentions generated column self-reference |
| Generated expression references another generated column | semantic error | Mentions generated-column dependencies |
| Generated column has `DEFAULT` | semantic error | Mentions DEFAULT is not allowed |
| Generated column has `ON UPDATE` | semantic error | Mentions ON UPDATE is not allowed |
| Generated column has `AUTO_INCREMENT` | semantic error | Mentions AUTO_INCREMENT is not allowed |
| INSERT/UPDATE assigns literal to generated column | semantic error | Mentions generated columns cannot be assigned |
| `ALTER TABLE ... GENERATED` | `DbError::NotImplemented` | Mentions ALTER generated columns |

## Edge cases

- [ ] Parser accepts `GENERATED ALWAYS AS (...) STORED` after ordinary column
      constraints.
- [ ] Catalog serializes and deserializes generated metadata without breaking
      old rows that do not have it.
- [ ] INSERT computes generated values for single-row VALUES.
- [ ] INSERT computes generated values for multi-row VALUES.
- [ ] INSERT computes generated values for `INSERT ... SELECT`.
- [ ] INSERT with explicit `DEFAULT` for a generated column computes the value.
- [ ] INSERT with explicit non-DEFAULT generated value errors.
- [ ] UPDATE recomputes generated values after changing base columns.
- [ ] UPDATE direct assignment to a generated column errors.
- [ ] `ON CONFLICT DO UPDATE`, ODKU, and MERGE update paths recompute values.
- [ ] `RETURNING *` includes computed generated values.
- [ ] CHECK and UNIQUE constraints can use STORED generated values.
- [ ] `VIRTUAL` DDL returns a clear `NotImplemented`.

## On-disk format

`axiom_columns` remains backward-compatible. Existing flag bits are:

```text
bit0 nullable
bit1 auto_increment
bit2 type_len present
bit3 is_fixed_len
bit4 default_expr present
bit5 on_update_expr present
```

This subphase uses:

```text
bit6 generated_expr present
bit7 generated kind: 0 = STORED, 1 = VIRTUAL
```

When bit6 is set, append the generated expression after `on_update_expr`:

```text
[generated_expr_len: u16 little-endian][generated_expr utf8 bytes]
```

Old catalog rows have bit6 clear and read as non-generated columns.

## Performance budget

| Operation | Target | Max acceptable |
|---|---:|---:|
| INSERT with one simple STORED generated column | no more than 5% slower than same table with manual value | 10% slower |
| UPDATE touching one dependency column | no more than one expression eval per generated column per row | no extra table scan |

The main Phase 21 gates remain `cargo test -p axiomdb-sql`, workspace tests,
and clippy. No comparison benchmark is required unless implementation changes
row storage or scan loops beyond write-time materialization.

## Dependencies

- Depends on Phase 21.5 INSERT/MERGE/UPSERT write paths.
- Depends on existing expression parser, expression SQL serialization, and
  default/on-update catalog-expression persistence.
- Blocks closing the generated-column gap in `docs/progreso.md`.

## Open questions

None. The accepted scope is STORED now, VIRTUAL later.

## Done criteria

- [ ] AST can represent generated-column metadata.
- [ ] Parser accepts STORED generated column syntax.
- [ ] `CREATE TABLE` persists generated metadata and rejects out-of-scope forms.
- [ ] Catalog serialization remains backward-compatible.
- [ ] INSERT paths materialize STORED generated columns.
- [ ] UPDATE / conflict-update paths recompute STORED generated columns.
- [ ] Direct writes to generated columns are rejected except `DEFAULT`.
- [ ] `VIRTUAL` and `ALTER TABLE ... GENERATED` return explicit
      `NotImplemented`.
- [ ] Integration tests cover parser, catalog, INSERT, UPDATE, conflict paths,
      RETURNING, and error cases.
- [ ] `cargo fmt --check` passes.
- [ ] `cargo test -p axiomdb-catalog` passes.
- [ ] `cargo test -p axiomdb-sql` passes.
- [ ] `cargo clippy -p axiomdb-sql -- -D warnings` passes.
- [ ] `python3 tools/wire-test.py` passes if wire-visible SQL is added there.

## References

- `docs/progreso.md` Phase 21.5f.
- `docs/gaps-mysql-compat.md` generated-column gap.
- `db.md` Phase 21 and generated-column design notes.
