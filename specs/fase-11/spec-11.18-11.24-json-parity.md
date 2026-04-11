# Spec: 11.18-11.24 JSON/JSONB Parity Roadmap

## Status

Planned. This file tracks the JSON/JSONB follow-up work after:

- 11.4: native text-backed `JSON`
- 11.16: binary `JSONB` + JSONPath functions
- 11.17: GIN acceleration for `JSONB @>` containment

The immediate goal is PostgreSQL-compatible JSONB parity for common operators,
then SQL/JSON standard query functions, then selected Oracle JSON compatibility
surfaces.

## References

- PostgreSQL local clone:
  - `research/postgresql/doc/src/sgml/json.sgml`
  - `research/postgresql/src/include/catalog/pg_operator.dat`
  - `research/postgresql/src/include/catalog/pg_proc.dat`
  - `research/postgresql/src/backend/utils/adt/jsonb_gin.c`
- Oracle JSON docs:
  - SQL/JSON query functions: `JSON_VALUE`, `JSON_QUERY`, `JSON_EXISTS`
  - `JSON_TABLE`
  - JSON data type, dot notation, JSON search index, Data Guide

## Current AxiomDB baseline

- `JSON` and `JSONB` column types.
- `JSON_EXTRACT`, `JSON_SET`, `JSON_REMOVE`, `JSON_KEYS`, `JSON_VALID`,
  `JSON_TYPE`, `JSON_MERGE_PATCH`, `JSON_CONTAINS`, `JSON_OVERLAPS`,
  `JSON_ARRAY_LENGTH`, `JSON_DEPTH`, `JSON_PRETTY`, `TO_JSONB`, `JSONB`.
- Operators `->`, `->>`, and `@>`.
- JSONPath functions `JSON_PATH_EXISTS`, `JSON_PATH_QUERY`,
  `JSON_PATH_QUERY_FIRST`.
- `CREATE INDEX ... USING GIN (jsonb_col)` for `@>` containment, with structural
  recheck.

## Subphases

### 11.18 PostgreSQL JSONB operator parity

Implement:

- Key-exists operators: `?`, `?|`, `?&`.
- Contained-by: `<@`.
- Concatenate: `||`.
- Delete operators: `-`, `#-`.
- Path extraction: `#>`, `#>>`.

Acceptance:

- Operators parse and evaluate on `JSONB`.
- NULL and type-error behavior is tested against PostgreSQL reference cases.
- Key-exists operators use GIN terms where a compatible index is present.

### 11.19 SQL/JSON standard query functions

Implement:

- `JSON_VALUE`
- `JSON_QUERY`
- `JSON_EXISTS`

Acceptance:

- Supports strict/lax path mode where applicable.
- Supports `RETURNING`, `PASSING`, `ON EMPTY`, and `ON ERROR` for the common
  cases needed by PostgreSQL/Oracle compatibility.
- Does not change existing MySQL-style `JSON_EXTRACT` behavior.

### 11.20 JSON_TABLE

Implement `JSON_TABLE(...)` as a row source in `FROM`.

Acceptance:

- Supports `COLUMNS`, scalar `PATH`, `EXISTS PATH`, `FOR ORDINALITY`, and
  `NESTED PATH`.
- Produces relational rows from JSON arrays and objects.
- Has integration tests for lateral-style use with a source table.

### 11.21 JSONPath parity and indexed path operators

Implement:

- PostgreSQL-style `@?` and `@@`.
- `jsonb_path_match`.
- `jsonb_path_query_array`.
- JSONPath variables for `PASSING`/vars-like arguments.
- Richer accessors and filters needed by PostgreSQL parity.
- `jsonb_path_ops` hash-based GIN operator class.

Acceptance:

- Planner can extract indexable JSONPath predicates for GIN where safe.
- `jsonb_path_ops` is selectable in `CREATE INDEX` and documented as a separate
  trade-off from the current `jsonb_ops`-style term index.

### 11.22 JSONB mutation parity

Implement:

- `jsonb_set`
- `jsonb_set_lax`
- `jsonb_insert`
- Field delete, array-element delete, and path delete semantics matching
  PostgreSQL operators/functions.

Acceptance:

- Clear tests documenting differences from MySQL `JSON_SET` and `JSON_REMOVE`.
- No in-place binary mutation is required in the first implementation; full blob
  rewrite is acceptable.

### 11.23 JSON Schema validation

Implement user-defined JSON schema validation for constraints and ad hoc checks.

Acceptance:

- Function-level validation, e.g. `JSON_SCHEMA_VALID` and validation report.
- Optional catalog storage for reusable schemas.
- `CHECK` constraint examples are covered by tests.

### 11.24 Oracle JSON compatibility surface

Research and implement the highest-value Oracle JSON features not covered by
the SQL/JSON standard subphases.

Candidates:

- Dot notation.
- `JSON_TRANSFORM`.
- `JSON_SERIALIZE`.
- `JSON_SCALAR`.
- `JSON_EQUAL`.
- Data Guide-style structure discovery.
- JSON search index full-text hooks.
- Document-collection APIs.
- JSON-relational duality view feasibility notes.

Acceptance:

- Split into smaller specs before implementation if any item is larger than a
  single focused subphase.
- Keep PostgreSQL behavior from 11.18-11.22 stable.

