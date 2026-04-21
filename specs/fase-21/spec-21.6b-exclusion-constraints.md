# Spec: 21.6b — Exclusion Constraints

Phase: 21 — Advanced SQL
Task: 21.6b exclusion constraints
Status: completed

## Context

Phase 21.6 closed CHECK constraints, but `docs/progreso.md` still tracks
exclusion constraints as the next table-integrity gap. The current parser and
catalog only model PRIMARY KEY / UNIQUE / FOREIGN KEY / CHECK, while the
executor already has mature UNIQUE-index enforcement paths on heap and
clustered tables.

## Goal

Implement the B-tree equality subset of `EXCLUDE USING` as a first-class table
constraint, enforced through an owned backing UNIQUE index.

## Non-goals

- GiST / SP-GiST / BRIN exclusion indexes.
- Range-overlap operators such as `&&`, range types, or `WITHOUT OVERLAPS`.
- Non-equality exclusion operators (`<>`, `<`, `<=`, `>`, `>=`).
- Expression elements, operator classes, collations, `ASC` / `DESC`, or
  `NULLS FIRST/LAST` inside exclusion elements.
- `WHERE` predicates on exclusion constraints.
- DEFERRABLE / INITIALLY DEFERRED semantics.
- Reusing a pre-existing user index instead of creating an owned backing index.

## Behavior

### Public API

The SQL AST and catalog must distinguish exclusion constraints from CHECK and
UNIQUE metadata:

```rust
pub enum ExclusionOperator {
    Eq,
}

pub struct ExclusionElement {
    pub column: String,
    pub operator: ExclusionOperator,
}

pub enum TableConstraint {
    // existing variants...
    Exclude {
        name: Option<String>,
        index_type: IndexType,
        elements: Vec<ExclusionElement>,
    },
}

pub enum ConstraintKind {
    Check,
    Exclusion,
}

pub struct ConstraintDef {
    pub kind: ConstraintKind,
    pub owned_index_id: u32,
    pub check_expr: String,
    pub exclude_elements: Vec<(u16, ExclusionOperator)>,
    // existing fields remain compatible
}
```

Exact type names may differ to match local style, but the catalog must persist:

- the constraint kind (`CHECK` vs `EXCLUSION`);
- the owned backing index id for exclusion constraints;
- the constrained column order and operator list.

### Grammar

Supported successful forms:

```sql
CREATE TABLE users (
    id INT PRIMARY KEY,
    slug TEXT,
    EXCLUDE USING btree (slug WITH =)
);

CREATE TABLE reservations (
    room_id INT,
    slot_id INT,
    CONSTRAINT reservations_room_slot_excl
        EXCLUDE USING btree (room_id WITH =, slot_id WITH =)
);

ALTER TABLE reservations
    ADD CONSTRAINT reservations_room_slot_excl
    EXCLUDE USING btree (room_id WITH =, slot_id WITH =);
```

Accepted grammar rules for 21.6b:

- `EXCLUDE USING btree (...)` only.
- Each element is `column_name WITH =`.
- `CREATE TABLE` may omit the constraint name and auto-generate one.
- `ALTER TABLE ... ADD CONSTRAINT` requires an explicit constraint name.

Rejected for this subphase:

```sql
EXCLUDE USING gist (room_id WITH =, during WITH &&)
EXCLUDE USING btree (room_id WITH <>)
EXCLUDE USING btree ((lower(email)) WITH =)
EXCLUDE USING btree (room_id WITH =) WHERE (status = 'active')
```

### Semantics

- An exclusion conflict occurs when all listed operators evaluate `TRUE`
  between the candidate row and an existing visible row.
- In 21.6b every supported operator is `=`, so conflicts reduce to
  tuple-equality across the listed columns.
- SQL NULL semantics apply: if any constrained column in either row is `NULL`,
  the equality comparison is `UNKNOWN`, so the row pair does not conflict.
- CREATE TABLE and ALTER ADD CONSTRAINT create an owned backing UNIQUE B-tree
  index over the same column tuple.
- Existing duplicate rows cause CREATE TABLE / ALTER ADD CONSTRAINT to fail.
- INSERT, UPDATE, REPLACE, `INSERT ... ON CONFLICT`, MySQL ODKU, and MERGE are
  enforced automatically by the backing UNIQUE index.
- Duplicate-key failures coming from the owned backing index must be translated
  to an exclusion-constraint error that names the table and constraint, not the
  helper index.
- DROP CONSTRAINT on an exclusion constraint removes both the catalog
  constraint row and the owned backing index.
- Information schema must surface the constraint as `EXCLUSION` and must not
  leak the helper index as an ordinary UNIQUE constraint in
  `TABLE_CONSTRAINTS` / `KEY_COLUMN_USAGE`.

### Error cases

| Input | Expected error | Message requirement |
|---|---|---|
| `EXCLUDE USING gist (...)` | `DbError::NotImplemented` | Mentions GiST exclusion constraints |
| `EXCLUDE USING btree (a WITH <>)` | `DbError::NotImplemented` | Mentions only `WITH =` is supported |
| `EXCLUDE USING hash (...)` | `DbError::NotImplemented` | Mentions `USING btree` |
| `EXCLUDE USING btree ((expr) WITH =)` | `DbError::NotImplemented` | Mentions expression elements |
| `EXCLUDE USING btree (...) WHERE (...)` | `DbError::NotImplemented` | Mentions exclusion predicates |
| Unknown constrained column | `DbError::ColumnNotFound` | Includes column name |
| Conflicting existing rows during ADD CONSTRAINT | exclusion violation | Names table + constraint |
| INSERT/UPDATE collides with existing tuple | exclusion violation | Names table + constraint |
| Duplicate constraint name | existing duplicate-name error | Includes constraint name |

## Edge cases

- [x] Anonymous `CREATE TABLE ... EXCLUDE USING btree (...)` auto-generates a
      stable constraint name and helper index name.
- [x] Multi-column equality exclusion rejects only when the full tuple matches.
- [x] Rows with `NULL` in any constrained column do not conflict.
- [x] UPDATE that changes constrained columns into a conflicting tuple errors.
- [x] UPDATE of unrelated columns does not error.
- [x] ALTER ADD CONSTRAINT validates existing rows before persisting metadata.
- [x] DROP CONSTRAINT removes the owned backing index.
- [x] `information_schema.TABLE_CONSTRAINTS` reports `EXCLUSION`.
- [x] `information_schema.KEY_COLUMN_USAGE` does not double-report the helper
      index as a UNIQUE constraint.
- [x] CHECK / FK / generated-column enforcement still observe the same final
      row state as before.

## On-disk format

`axiom_constraints` must remain backward-compatible with existing CHECK rows.
The current row body is:

```text
[constraint_id][table_id][name_len][name][expr_len][check_expr]
```

This subphase adds an optional trailer:

```text
[kind: u8]
if kind == CHECK:
    no extra payload
if kind == EXCLUSION:
    [owned_index_id: u32 LE]
    [index_type: u8]            // 0 = btree for now
    [num_elements: u8]
    repeated num_elements times:
        [col_idx: u16 LE]
        [operator: u8]          // 0 = Eq
```

Compatibility rule:

- Legacy rows with no trailer decode as `ConstraintKind::Check`.
- New readers must accept both old CHECK rows and new tagged rows.
- New writers may continue storing `check_expr` as the existing field for CHECK
  and an empty string for exclusion rows.

## Performance budget

| Operation | Target | Max acceptable |
|-----------|--------|----------------|
| INSERT/UPDATE on excluded columns | Same as equivalent UNIQUE index path | No more than 5% slower |
| CREATE/ALTER ADD exclusion constraint | Same order as CREATE UNIQUE INDEX | No extra heap scan beyond index build / validation |

Reference: UNIQUE index enforcement and maintenance paths already closed in
earlier phases and are the baseline for this subphase.

## Dependencies

- Depends on existing UNIQUE-index creation / maintenance on heap and
  clustered tables.
- Depends on ALTER TABLE ADD/DROP CONSTRAINT plumbing from CHECK / FK work.
- Blocks closing Phase 21.6b in `docs/progreso.md`.

## Open questions

None. The chosen scope is equality-only `USING btree` with owned helper index;
GiST / range overlap remain deferred.

## Done criteria

- [x] AST can represent `EXCLUDE USING btree (... WITH = ...)`.
- [x] Parser accepts CREATE TABLE and ALTER ADD CONSTRAINT exclusion syntax.
- [x] DDL rejects unsupported index types, operators, expressions, and
      predicates with explicit `NotImplemented`.
- [x] Catalog persists exclusion metadata and remains backward-compatible with
      existing CHECK rows.
- [x] CREATE TABLE / ALTER ADD CONSTRAINT create and own a backing UNIQUE index.
- [x] INSERT / UPDATE-like paths surface exclusion failures as exclusion
      violations, not raw helper-index unique violations.
- [x] DROP CONSTRAINT removes the owned backing index.
- [x] Information schema reports exclusion constraints correctly without
      double-reporting the helper index as UNIQUE.
- [x] Integration tests cover parser, CREATE TABLE, ALTER ADD, NULL semantics,
      UPDATE conflicts, DROP CONSTRAINT, and metadata visibility.
- [x] `cargo fmt --check` passes.
- [x] `cargo test -p axiomdb-catalog` passes.
- [x] `cargo test -p axiomdb-sql` passes.
- [x] `cargo clippy -p axiomdb-sql -- -D warnings` passes.
- [x] `python3 tools/wire-test.py` passes if the SQL surface is added there.

## References

- `docs/progreso.md` Phase 21.6b.
- `memory/project_state.md` next-subphase note.
- Existing CHECK constraint flow in `crates/axiomdb-sql/src/executor/ddl_alter_constraint.rs`.
- Existing UNIQUE-index enforcement in `crates/axiomdb-sql/src/index_maintenance.rs`.
