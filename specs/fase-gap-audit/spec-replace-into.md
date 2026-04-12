# Spec: REPLACE INTO (MySQL upsert)

## What to build (not how)

MySQL's `REPLACE INTO` statement — a row-level upsert that behaves as
"delete every row that would violate a PRIMARY KEY or UNIQUE constraint,
then insert the new row". It coexists with `INSERT`, shares the
VALUES / DEFAULT VALUES / SET / SELECT sources, and honors `DEFAULT`,
`AUTO_INCREMENT`, composite unique indexes, partial indexes, and
FK cascade semantics on the displaced rows.

Behavior must match MariaDB's observable semantics (researched in
`sql/sql_insert.cc::replace_row`) at statement level, while using
AxiomDB's proactive lookup model (not the retry-on-error loop) for
determinism and cleaner error paths.

## Inputs / Outputs

### Inputs

```sql
REPLACE [INTO] table_ref
    [(col1, col2, ...)]
    {VALUES (expr,...), (...) | DEFAULT VALUES | SET col=val, ... | SELECT ...}
```

Also accepted (MariaDB extension): `REPLACE LOW_PRIORITY|DELAYED INTO ...`
— priority modifiers consumed and discarded (matches `INSERT`).

**Not accepted** (parser error):
- `REPLACE IGNORE` — mutually exclusive with REPLACE in MySQL.

### Outputs

`QueryResult::Affected { count, last_insert_id }`

- `count = inserted_rows + deleted_rows` (MariaDB formula; match exactly).
- `last_insert_id`: first AUTO_INCREMENT value generated in the statement
  (same rule as INSERT). For conflicts that do NOT generate a new AI
  value (because the user supplied one and it matched an existing row),
  `last_insert_id` stays at 0 / previous value.

## Use cases

1. **Idempotent catalog upsert**: loading a reference table from an
   external feed. `REPLACE INTO countries VALUES (1,'US'),(2,'MX')`
   — any pre-existing rows with `country_id=1` or `country_id=2` are
   overwritten.
2. **Cache write-through**: session/token stores keyed by a UNIQUE
   column, where newer values supersede old without explicit deletes.
3. **Compatibility with ORMs/migrations** that emit REPLACE (Doctrine,
   older Rails migration patterns, custom PHP code).
4. **Composite unique replacement**: `REPLACE INTO user_tag(user_id, tag)
   VALUES (...)` — displaces the existing (user, tag) pair.
5. **FK-cascade displacement**: replacing a parent row triggers
   `ON DELETE CASCADE` on its children (match `DELETE` semantics).

## Acceptance criteria

- [ ] **Parsing**: `REPLACE` is recognized as a top-level statement
      verb only at the start of a statement; it keeps working as the
      `REPLACE(str, from, to)` scalar function in expression contexts.
- [ ] **Parsing — all source forms**:
  - `REPLACE INTO t VALUES (...)`,
  - `REPLACE INTO t (a, b) VALUES (...)`,
  - `REPLACE INTO t SET a = 1, b = 'x'`,
  - `REPLACE INTO t DEFAULT VALUES`,
  - `REPLACE INTO t SELECT * FROM s`,
  - `REPLACE LOW_PRIORITY INTO t VALUES (...)`.
- [ ] **Parser rejects `REPLACE IGNORE`** with a clear error.
- [ ] **No-conflict row is a plain INSERT**: `affected_rows = 1`, all
      secondary indexes updated, FK child-insert validation runs.
- [ ] **Conflict on PRIMARY KEY**: the pre-existing row is deleted
      (FK cascade applies), new row inserted. `affected_rows = 2`.
- [ ] **Conflict on non-PK UNIQUE index**: same as PK conflict.
- [ ] **Conflict on composite UNIQUE index** (all key columns
      non-NULL): displaces the matching row. `affected_rows = 2`.
- [ ] **NULL in a UNIQUE key column**: no conflict, behaves as INSERT.
      (Matches SQL standard / MariaDB.)
- [ ] **Partial UNIQUE index**: a row that satisfies the partial
      predicate and matches an existing row under the same predicate
      displaces it; rows outside the predicate never conflict on that
      index.
- [ ] **Multi-index conflict (new row displaces two different rows
      via two unique indexes)**: all conflicting rows are deleted,
      new row inserted. `affected_rows = 1 + deleted_rows`.
- [ ] **AUTO_INCREMENT rejection rule**: if the AI column has a
      user-supplied value AND that value conflicts on the AI index
      AND the AI column is the conflicting column, return an error
      (matches MariaDB's `HA_ERR_AUTOINC_ERANGE` prevention of key
      exhaustion).
- [ ] **FK cascade on displaced row**: `ON DELETE CASCADE/SET NULL/
      SET DEFAULT` actions on children fire exactly as they would
      for a plain `DELETE`.
- [ ] **FK `ON DELETE RESTRICT/NO ACTION`**: if the displaced parent
      has children that would violate RESTRICT, the REPLACE errors
      out (no partial state — the failed row's insert is rolled back).
- [ ] **Clustered tables**: REPLACE works identically on clustered
      tables (not deferred). The delete+insert round-trips through
      `update_clustered_insert` / `update_clustered_delete`.
- [ ] **Heap tables**: identical behavior via the existing
      `TableEngine::delete_row` + insert path.
- [ ] **SELECT source with self-reference** (`REPLACE INTO t
      SELECT * FROM t WHERE ...`): the SELECT is materialized
      before the first displace, so every row the SELECT saw is
      processed even if later displaced.
- [ ] **`affected_rows` formula**: `inserted + deleted` — verified in
      integration tests (including the 2-per-conflict-row case).
- [ ] **`last_insert_id`** follows INSERT rules.
- [ ] **Integration tests in `tests/integration_replace_into.rs`**
      cover every bullet above.
- [ ] `cargo test --workspace` clean.
- [ ] `cargo clippy --workspace -- -D warnings` clean.
- [ ] `cargo fmt --check` clean.

## Out of scope

- **Last-unique-index UPDATE-in-place optimization** (MariaDB
  preserves row identity when the conflict is on the LAST unique index).
  Deferred as a performance follow-up. MVP always does DELETE+INSERT
  so FK children of the displaced parent are re-cascaded.
- **Triggers** (`BEFORE/AFTER DELETE/INSERT`) — AxiomDB has no
  trigger infrastructure yet. When triggers land (Phase 16.3), the
  delete+insert pair must fire them in MariaDB order; REPLACE will
  inherit this for free because it reuses `TableEngine::delete_row`
  and the insert path.
- **Binlog / replication format** — AxiomDB has no replication layer.
- **`REPLACE ... RETURNING`** (PostgreSQL extension, not MySQL).
- **PostgreSQL-style `INSERT ... ON CONFLICT DO UPDATE`** — separate
  feature, spec-worthy on its own (GAP follow-up).

## Dependencies

- Existing INSERT path (`executor/insert_heap_ctx.rs`,
  `executor/insert_clustered_ctx.rs`).
- Existing DELETE path with FK cascade (`fk_enforcement.rs` +
  `TableEngine::delete_row` + clustered delete).
- Existing unique-index lookup (`BTree::lookup_in`,
  `clustered_tree::lookup`).
- Composite-key encoding (`encode_index_key`).
- Partial-index predicate evaluation (`partial_index::compile_index_predicates`).
- `DbError::UniqueViolation` variant (already present).
- New `DbError` variant for the AUTO_INCREMENT rejection rule
  (or reuse an existing invalid-value error with a specific message).

## Effort for next step

- **Plan: low** — the design is mostly "wrap INSERT with a
  pre-lookup + delete step"; touches at most `ast.rs`, `parser/dml.rs`,
  `parser/mod.rs`, `executor/insert_heap_ctx.rs`,
  `executor/insert_clustered_ctx.rs`, and one new test file.
