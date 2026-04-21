# Plan: 21.6b — Exclusion Constraints

Phase: 21 — Advanced SQL
Task: 21.6b exclusion constraints
Spec: specs/fase-21/spec-21.6b-exclusion-constraints.md
Status: completed

## Summary

Implement `EXCLUDE USING btree (... WITH = ...)` in the lowest-risk order:
parser and AST first, then catalog and error-surface plumbing, then DDL that
creates an owned backing UNIQUE index, and finally metadata visibility and
drop-path cleanup. The key design choice is to reuse existing UNIQUE-index
enforcement instead of inventing a new row-to-row scan engine for 21.6b.

## Dependencies

Must be done first:
- [x] Phase 21.6 CHECK constraints closed.
- [x] UNIQUE-index creation / maintenance already available on current table
      layouts.
- [x] `specs/fase-21/spec-21.6b-exclusion-constraints.md` approved.

Blocks:
- [x] Closing Phase 21.6b in `docs/progreso.md`.
- [ ] Any later GiST/range-overlap exclusion work that needs a stable AST and
      catalog shape.

## Affected files

New files:
- `crates/axiomdb-sql/tests/integration_exclusion_constraints.rs` — end-to-end
  DDL and DML coverage for exclusion constraints.

Modified files:
- `crates/axiomdb-sql/src/ast.rs` — add exclusion-constraint AST metadata.
- `crates/axiomdb-sql/src/lexer.rs` — add `EXCLUDE` token if needed.
- `crates/axiomdb-sql/src/parser/ddl.rs` — parse `EXCLUDE USING btree (...)`.
- `crates/axiomdb-sql/tests/integration_ddl_parser.rs` — parser-only coverage.
- `crates/axiomdb-core/src/error.rs` — add exclusion-violation error variant.
- `crates/axiomdb-core/src/error_response.rs` — detail / hint / SQLSTATE.
- `crates/axiomdb-network/src/mysql/error.rs` — MySQL-wire mapping.
- `crates/axiomdb-sql/src/session.rs` — classify exclusion violations as
  user-facing / ignorable where appropriate.
- `crates/axiomdb-catalog/src/schema_constraints.rs` — extend constraint row
  format for kind + owned index + exclusion elements.
- `crates/axiomdb-catalog/src/reader.rs` and
  `crates/axiomdb-catalog/src/writer.rs` — read / write exclusion metadata.
- `crates/axiomdb-sql/src/executor/ddl_create_table.rs` — CREATE TABLE support.
- `crates/axiomdb-sql/src/executor/ddl_alter_constraint.rs` — ALTER ADD/DROP
  support and owned-index cleanup.
- `crates/axiomdb-sql/src/executor/information_schema_exec.rs` — surface
  EXCLUSION and hide helper indexes from UNIQUE metadata views.
- `docs/progreso.md`, `memory/project_state.md`, `memory/architecture.md`,
  `memory/lessons.md`, `docs/fase-21.md` — close subphase after gates.

## Step 1 — Parser and AST

Status: completed.

**Goal:** represent exclusion constraints in the SQL AST and parser.
**Files:** `ast.rs`, `lexer.rs`, `parser/ddl.rs`,
`tests/integration_ddl_parser.rs`.
**Approach:** TDD with parser tests first.

### Tests to add

```rust
#[test]
fn parse_create_table_exclude_btree_eq() { ... }

#[test]
fn parse_named_alter_add_exclude_btree_eq() { ... }
```

### Implementation outline

- Add `TableConstraint::Exclude`.
- Add the `EXCLUDE` lexer token.
- Parse `EXCLUDE USING btree (<col> WITH = [, ...])`.
- Keep unsupported operators / index types syntactically representable where
  practical so DDL can reject them explicitly.

### Verification

```bash
cargo test -p axiomdb-sql --test integration_ddl_parser
```

## Step 2 — Catalog and error surface

Status: completed.

**Goal:** persist exclusion metadata and add a dedicated surfaced error.
**Files:** `axiomdb-core/src/error.rs`, `error_response.rs`,
`network/src/mysql/error.rs`, `session.rs`,
`schema_constraints.rs`, `reader.rs`, `writer.rs`.
**Approach:** round-trip tests and error-mapping tests first.

### Tests to add

```rust
#[test]
fn constraint_def_roundtrips_exclusion_metadata() { ... }

#[test]
fn exclusion_violation_has_stable_sqlstate_and_message() { ... }
```

### Implementation outline

- Extend `ConstraintDef` with a backward-compatible tagged trailer.
- Add an exclusion-violation `DbError` variant.
- Update core / wire error mapping and session classification.
- Preserve existing CHECK-row decoding.

### Verification

```bash
cargo test -p axiomdb-catalog
cargo test -p axiomdb-sql --test integration_errors
```

## Step 3 — CREATE / ALTER with owned backing UNIQUE index

Status: completed.

**Goal:** create exclusion constraints by creating and owning a helper UNIQUE
index, then translate helper-index duplicates into exclusion violations.
**Files:** `ddl_create_table.rs`, `ddl_alter_constraint.rs`,
`integration_exclusion_constraints.rs`.
**Approach:** end-to-end DDL tests first.

### Tests to add

```rust
#[test]
fn create_table_exclude_rejects_duplicate_existing_tuple() { ... }

#[test]
fn alter_add_exclude_rejects_existing_conflict() { ... }

#[test]
fn insert_conflict_surfaces_exclusion_violation() { ... }

#[test]
fn update_conflict_surfaces_exclusion_violation() { ... }
```

### Implementation outline

- Validate `USING btree`, `WITH =`, and column-only elements.
- Auto-generate a stable constraint name and helper index name when omitted in
  CREATE TABLE.
- Create a helper UNIQUE index over the same column tuple.
- Persist exclusion metadata with the owned helper index id.
- Translate `UniqueViolation` from owned helper indexes into
  `ExclusionViolation`.
- Ensure DROP CONSTRAINT removes both the constraint row and its owned index.

### Verification

```bash
cargo test -p axiomdb-sql --test integration_exclusion_constraints
cargo test -p axiomdb-sql --test integration_executor_ddl
```

## Step 4 — Information schema and metadata cleanup

Status: completed.

**Goal:** surface exclusion constraints as `EXCLUSION` without leaking helper
indexes as ordinary UNIQUE constraints.
**Files:** `information_schema_exec.rs`,
`integration_exclusion_constraints.rs`.
**Approach:** metadata-query tests first.

### Tests to add

```rust
#[test]
fn information_schema_reports_exclusion_not_unique() { ... }

#[test]
fn drop_constraint_removes_helper_index_metadata() { ... }
```

### Implementation outline

- Teach `TABLE_CONSTRAINTS` to emit exclusion rows from catalog constraints.
- Filter helper indexes owned by exclusion constraints out of UNIQUE reporting.
- Apply the same filtering to `KEY_COLUMN_USAGE`.

### Verification

```bash
cargo test -p axiomdb-sql --test integration_exclusion_constraints
cargo test -p axiomdb-sql --test integration_information_schema
```

## Step 5 — Final integration and closure

Status: completed.

**Goal:** run gates, update project docs, and close 21.6b.
**Files:** docs + memory files listed above.

### Verification against spec

- [x] AST can represent `EXCLUDE USING btree (... WITH = ...)`.
- [x] Parser accepts CREATE TABLE and ALTER ADD CONSTRAINT exclusion syntax.
- [x] DDL rejects unsupported index types, operators, expressions, and
      predicates with explicit `NotImplemented`.
- [x] Catalog persists exclusion metadata and remains backward-compatible with
      existing CHECK rows.
- [x] CREATE TABLE / ALTER ADD CONSTRAINT create and own a backing UNIQUE
      index.
- [x] INSERT / UPDATE-like paths surface exclusion failures as exclusion
      violations, not raw helper-index unique violations.
- [x] DROP CONSTRAINT removes the owned backing index.
- [x] Information schema reports exclusion constraints correctly without
      double-reporting the helper index as UNIQUE.
- [x] Integration tests cover parser, CREATE TABLE, ALTER ADD, NULL semantics,
      UPDATE conflicts, DROP CONSTRAINT, and metadata visibility.
- [x] `cargo fmt --check`
- [x] `cargo test -p axiomdb-catalog`
- [x] `cargo test -p axiomdb-sql`
- [x] `cargo clippy -p axiomdb-sql -- -D warnings`
- [x] `python3 tools/wire-test.py` if updated

## Risk register

| Risk | Likelihood | Mitigation |
|---|---:|---|
| Helper UNIQUE index leaks as UNIQUE metadata | medium | Persist owned index id and filter helper indexes in IS views |
| UniqueViolation translation misses a write path | medium | Centralize translation near DDL-owned index lookup; test INSERT, UPDATE, ODKU/ON CONFLICT if affected |
| Constraint row format breaks old CHECK rows | low | Use appended trailer with legacy default-to-CHECK decoding |
| Scope expands into GiST/range work | medium | Reject non-btree / non-equality syntax immediately with explicit `NotImplemented` |

## Rollback plan

If implementation stalls mid-way:

1. Revert partial AST / catalog changes together so parser and catalog formats
   stay aligned.
2. Leave the draft spec / plan in place with an updated note on the blocker.
3. Keep any helper-index experiments off `main` until end-to-end translation and
   DROP CONSTRAINT cleanup both pass.

## Estimated effort

Total: high.

- Step 1: 30-45 min
- Step 2: 45-60 min
- Step 3: 1.5-2.5 h
- Step 4: 30-60 min
- Step 5: 30-45 min
