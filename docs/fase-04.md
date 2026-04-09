# Phase 04 — SQL Parser + Executor

## 2026-04-09 — ALTER TABLE closure batch

### 4.22c — `ALTER TABLE ... ADD PRIMARY KEY (...)`

- heap tables can now be promoted to clustered storage with a single ALTER
- existing rows are validated for `NULL` and duplicate key tuples before rebuild
- primary-key columns become `NOT NULL` in catalog metadata
- existing secondary indexes survive the promotion and are rebuilt as clustered
  bookmark indexes

### 4.22e — indexed `DROP COLUMN` / `MODIFY COLUMN`

- `DROP COLUMN` now auto-drops secondary indexes whose definition depends on the
  removed column
- heap rewrites rebuild surviving secondary indexes because physical `RecordId`
  bookmarks change
- clustered rewrites only rebuild affected secondary indexes
- preserved index metadata includes partial predicates, `INCLUDE` columns,
  `fillfactor`, index type, and BRIN `pages_per_range`
- PRIMARY KEY / FOREIGN KEY / CHECK-dependent columns still reject explicitly

### Validation

- `cargo test --workspace --no-fail-fast`
- `cargo clippy --workspace -- -D warnings`
- `cargo fmt --check`
- `tools/wire-test.py`: `338/338`
