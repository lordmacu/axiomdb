# Spec: 20.3 — ENUMs

Phase: 20 — Types + import/export
Task: 20.3 ENUMs
Status: approved

## Context

Phase 20.2 added catalog-backed sequences. Phase 20.3 adds user-defined ENUM
types so schemas can constrain a text-like column to a fixed ordered label set.

AxiomDB currently stores row values with a closed physical `Value` / `DataType`
/ `ColumnType` set. This subphase uses a catalog-backed ENUM type whose column
values are stored as `TEXT`, preserving the existing row codec and on-disk row
format while adding SQL-level validation and declared-order semantics.

## Goal

Implement PostgreSQL-style `CREATE TYPE ... AS ENUM` and use those ENUM types
as column types with validation, metadata persistence, and semantic ordering.

## Non-goals

- Not changing the row codec to store ENUM values as compact `u32` ordinals in
  this subphase; physical compact storage is deferred to a later type-system
  optimization phase.
- Not implementing `ALTER TYPE ... ADD VALUE`, `RENAME VALUE`, `RENAME TYPE`,
  or dependency-aware type evolution.
- Not implementing `DROP TYPE` in this subphase unless it can be delivered with
  complete dependency checks. A parser/runtime `NotImplemented` is acceptable
  if documented.
- Not implementing MySQL inline `ENUM('a','b')` column syntax; this subphase is
  scoped to named PostgreSQL-style enum types.
- Not implementing arrays of enum values; Phase 20.4 handles arrays.
- Not implementing cross-database enum references beyond the existing
  database/schema name resolution model.

## Public SQL Surface

Accepted DDL:

```sql
CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');
CREATE TYPE public.status AS ENUM ('new', 'open', 'closed');

CREATE TABLE tasks (
  id BIGINT PRIMARY KEY,
  state status NOT NULL
);
```

Accepted DML / queries:

```sql
INSERT INTO tasks VALUES (1, 'new');
UPDATE tasks SET state = 'closed' WHERE id = 1;
SELECT * FROM tasks WHERE state = 'open';
SELECT * FROM tasks ORDER BY state;
```

Optional if implementation cost stays bounded:

```sql
CAST('open' AS status)
```

## Semantics

- `CREATE TYPE name AS ENUM (...)` creates a named enum type in the current
  schema, unless the type name already exists.
- Enum labels are case-sensitive text values.
- Enum labels must be unique within a type.
- Enum labels are ordered by declaration order. The first label has ordinal 1,
  the second ordinal 2, and so on.
- A column declared with an enum type accepts:
  - SQL `NULL` if the column is nullable.
  - Text values whose exact string matches one of the enum labels.
- A write that supplies a non-label text value to an enum column fails before
  the row is inserted or updated.
- Existing table write paths must all enforce the same rule where they already
  enforce column type coercion: INSERT, INSERT SELECT, UPDATE, ODKU, ON
  CONFLICT, MERGE, generated stored values, and clustered/heap paths that use
  the shared column coercion helpers.
- Equality comparisons on enum columns compare label text for SQL-visible
  equality.
- Ordering comparisons and `ORDER BY` on enum-typed columns use declaration
  ordinal when both operands/ordering expressions resolve to the same enum
  type. If ordinal context cannot be proven, the implementation may fall back
  to text ordering only if this is explicitly documented and tested.
- `SHOW CREATE TABLE`, `DESCRIBE`, `SHOW FULL COLUMNS`, and
  `information_schema.COLUMNS` report the declared enum type name rather than
  plain `TEXT` where the catalog can identify it.
- Database reopen preserves enum type definitions and labels.

## Catalog Metadata

Persist one row per enum type with at least:

- schema name
- type name
- ordered labels

The catalog representation must preserve label order and exact label bytes as
UTF-8 text. It must be backward compatible for databases created before 20.3;
legacy databases lazily initialize the enum catalog root.

Column metadata must preserve the declared enum type identity. The physical
storage type may remain `ColumnType::Text`, but schema resolution must retain
enough metadata to validate writes and report the declared type.

## Error Cases

| Input | Expected error |
|-------|----------------|
| duplicate `CREATE TYPE mood AS ENUM (...)` | type already exists |
| empty enum label list | parse or invalid-value error |
| duplicate enum label | invalid enum definition |
| non-text enum label | parse error |
| unknown type in `CREATE TABLE t (c missing_enum)` | unknown type |
| insert `'bad'` into enum column | invalid enum value |
| update enum column to `'bad'` | invalid enum value |
| comparing/order between different enum types | deterministic error or documented text fallback |
| `DROP TYPE mood` while used by a table | not implemented or dependency error |

## Edge Cases

- [ ] Enum labels may contain spaces and punctuation.
- [ ] Enum labels are case-sensitive (`'open'` and `'OPEN'` are different).
- [ ] Duplicate labels are rejected exactly, not case-insensitively.
- [ ] Empty label list is rejected.
- [ ] Nullable enum columns accept `NULL`.
- [ ] Defaults on enum columns are validated.
- [ ] Invalid enum values fail without partially writing multi-row statements.
- [ ] Enum metadata survives database reopen.
- [ ] `ORDER BY enum_col` follows declaration order.
- [ ] Index predicates and equality probes still work because physical values
      remain stored as text.
- [ ] `SHOW CREATE TABLE` and information schema expose enum type identity.

## On-disk Format

This subphase adds catalog metadata and column metadata, not a new row-value
encoding. User rows continue storing enum values as text bytes through the
existing `TEXT` codec path.

Enum catalog row format:

```text
[schema_len:1][schema UTF-8][name_len:1][name UTF-8][label_count:2 LE]
repeated label_count times:
  [label_len:2 LE][label UTF-8]
```

Compatibility rule: old databases have no enum catalog root and must lazily
allocate it. Future compact physical enum storage must be able to read these
catalog definitions and migrate/interpret text-backed enum columns.

## Performance Budget

| Operation | Target | Max acceptable |
|-----------|--------|----------------|
| enum value validation on INSERT | no more than +10% vs TEXT insert | +20% |
| enum equality predicate | same as TEXT equality when no ordinal needed | +10% |
| enum ORDER BY ordinal mapping | O(rows + labels) for one enum column | O(rows × labels) rejected |

Validation must use a set/map per enum type rather than scanning labels for
every row when inserting many rows.

## Dependencies

- Depends on Phase 20.2 catalog DDL patterns for object metadata roots.
- Depends on existing table column metadata and shared coercion helpers.
- Blocks Phase 20.4 enum arrays and later compact enum storage work.

## Open Questions

Resolved by brainstorm:

- Approach: catalog-backed enum type stored physically as text for this
  subphase.
- Compact ordinal storage: deferred.
- Inline MySQL `ENUM(...)`: deferred.

## Done Criteria

- [ ] Parser / AST accept `CREATE TYPE name AS ENUM (...)`.
- [ ] Catalog persists enum type definitions and ordered labels.
- [ ] Table DDL accepts a named enum type as a column type.
- [ ] Column metadata preserves declared enum type identity.
- [ ] INSERT, UPDATE, INSERT SELECT, ODKU, ON CONFLICT, and MERGE validate enum
      values on supported heap/clustered paths through shared helpers.
- [ ] Defaults on enum columns are validated.
- [ ] `ORDER BY enum_col` follows declaration order for enum-typed columns.
- [ ] Equality predicates on enum columns still work with existing text-backed
      indexes.
- [ ] Metadata reporting shows enum type names in `SHOW CREATE TABLE`,
      `DESCRIBE`, `SHOW FULL COLUMNS`, and `information_schema.COLUMNS`.
- [ ] Integration coverage exists for DDL, valid writes, invalid writes,
      defaults, ordering, metadata, persistence, and multi-row rollback.
- [ ] Wire smoke includes a bounded `[20.3 enums]` scenario.
- [ ] User docs and internals docs describe the text-backed enum contract and
      deferred compact storage.
- [ ] `cargo test -p axiomdb-catalog enum` passes.
- [ ] `cargo test -p axiomdb-sql --test integration_enums` passes.
- [ ] `python3 tools/wire-test.py` passes.
- [ ] `cargo test --workspace` passes at subphase close.
- [ ] `cargo clippy --workspace -- -D warnings` passes at subphase close.

## References

- `docs/fase-20.md`
- `docs/progreso.md` Phase 20.3
- `db.md` Type System — `ENUM` planned in Phase 20
- PostgreSQL `CREATE TYPE ... AS ENUM` behavior
- MySQL inline `ENUM(...)` behavior, intentionally deferred here
