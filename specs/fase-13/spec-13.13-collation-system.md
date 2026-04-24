# Spec: 13.13 — Collation system

Phase: 13 — Advanced PostgreSQL
Task: 13.13 Collation system
Status: draft

## Context

`docs/progreso.md` still describes `13.13` as a layered, ICU-backed collation
system with per-session, per-database, per-table, per-column, and per-query
overrides. The current repo is materially narrower:

- executor-visible text semantics are only `binary` and `es`
- `SET AXIOM_COMPAT` and `SET collation` already exist at session scope
- parser accepts many `COLLATE` / `CHARACTER SET` clauses but mostly discards
  them
- no database/table/column collation metadata is persisted in catalog
- no true ICU/UCA root engine exists despite stale docs claiming otherwise

That means `13.13` should not be framed as “full ICU parity lands now”. The
real deliverable must be the first honest, end-to-end collation system layer
that the existing runtime can actually honor.

## Goal

Deliver a real layered collation system over the **existing** executor
collations (`binary`, `es`), with persisted metadata and deterministic
precedence:

1. session default (`AXIOM_COMPAT` / `SET collation`)
2. database default collation
3. table default collation
4. column collation
5. query-time `... COLLATE ...` override

The result should be usable, catalog-backed, and wire-visible without promising
full ICU locale tailoring yet.

## Non-goals

- Not implementing full ICU / CLDR integration in this subphase.
- Not implementing a true UCA root algorithm.
- Not implementing locale-specific tailorings like `tr_TR`, `de_DE`, `ja_JP`.
- Not changing on-disk B-Tree sort keys to become collation-aware.
- Not making text indexes usable under non-binary collations in this slice.
- Not implementing automatic script detection (`COLLATE 'auto'`).
- Not implementing custom user-defined collations.

## Public SQL surface

### Accepted and enforced

```sql
CREATE DATABASE db COLLATE binary;
CREATE DATABASE db COLLATE es;

CREATE TABLE t (
  name TEXT COLLATE es,
  city TEXT
) COLLATE binary;

SELECT name FROM t ORDER BY name COLLATE es;
SELECT * FROM t WHERE name COLLATE es = 'josé';
```

### Existing session surface retained

```sql
SET AXIOM_COMPAT = 'mysql' | 'postgresql' | 'standard' | DEFAULT;
SET collation = 'binary' | 'es' | DEFAULT;
SELECT @@collation;
SELECT @@axiom_compat;
SHOW COLLATION;
```

## Collation precedence

Effective text semantics for an expression resolve in this order:

1. explicit query `... COLLATE ...`
2. referenced column collation
3. table default collation
4. database default collation
5. explicit session `SET collation`
6. compat-derived session default (`AXIOM_COMPAT`)
7. fallback `binary`

If multiple input expressions in one comparison have incompatible explicit
non-binary collations, return a clear user-facing error instead of guessing.

## Supported collations in 13.13

- `binary`
- `es`

Plus aliases accepted and normalized at parse / DDL boundaries:

- `C` → `binary`
- `utf8mb4_bin` → `binary`
- `utf8mb4_0900_ai_ci` → `es`
- `utf8mb4_general_ci` → `es`
- `utf8mb4_unicode_ci` → `es`

This gives migration value now without pretending locale-specific ICU semantics.

## Catalog/runtime format

Persist normalized collation names in:

- database definition
- table definition
- column definition

Table and column collations may be absent, meaning “inherit from parent scope”.

## Executor semantics

- All text comparison, `LIKE`, `ORDER BY`, `GROUP BY`, `DISTINCT`,
  `COUNT(DISTINCT)`, `MIN/MAX(TEXT)`, and grouping hash keys use the effective
  collation resolved for the participating expression(s).
- Query-time `COLLATE` only affects the expression where it appears.
- Existing safety rule remains: text indexes are not used when the effective
  collation is non-binary.
- `SHOW CREATE TABLE` reconstructs persisted table/column `COLLATE` clauses.
- `information_schema` / `SHOW FULL COLUMNS` expose effective metadata
  honestly enough for ORM introspection.

## Error cases

| Input | Expected error |
|-------|----------------|
| unknown collation name | `DbError::InvalidValue` |
| conflicting explicit collations in one comparison | `DbError::InvalidValue` |
| non-text expression with `COLLATE` | `DbError::TypeMismatch` or equivalent semantic error |

## Edge cases

- [ ] column with no explicit collation inherits table/database/session default
- [ ] query `COLLATE` overrides column default
- [ ] `AXIOM_COMPAT='mysql'` still derives `es` only when no explicit override exists
- [ ] `SHOW CREATE TABLE` round-trips table and column collations
- [ ] text index plans stay disabled under non-binary effective collation
- [ ] binary and es aliases normalize to canonical names in metadata

## Recommended cut for 13.13

Close `13.13` on the layered system above, and explicitly defer the old
roadmap items:

- `13.13a` UCA root default
- `13.13d` auto script detection
- `13.13e` full ICU integration

Those belong to a later collation-completeness phase once the metadata and
precedence model already exist.

## Done criteria

- [ ] normalized collation registry exists for `binary` / `es` + aliases
- [ ] database/table/column collation metadata persists in catalog
- [ ] parser stops discarding relevant `COLLATE` clauses and binds them into AST
- [ ] query-time `expr COLLATE name` works in comparisons / ORDER BY
- [ ] executor resolves effective collation with deterministic precedence
- [ ] `SHOW CREATE TABLE` / metadata introspection reflect persisted collations
- [ ] docs stop claiming ICU/UCA root as already implemented
- [ ] dedicated SQL integration coverage exists
- [ ] wire smoke includes one bounded `13.13` scenario
- [ ] `cargo test -p axiomdb-sql` for touched tests passes
- [ ] `python3 tools/wire-test.py` passes
- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace -- -D warnings` passes

## Files reviewed

- `crates/axiomdb-sql/src/session.rs`
- `crates/axiomdb-sql/src/text_semantics.rs`
- `crates/axiomdb-sql/src/parser/ddl.rs`
- `docs/progreso.md`
- `docs-site/src/user-guide/sql-reference/dml.md`
- `docs-site/src/development/decisions.md`

