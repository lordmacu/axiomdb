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

## 2026-04-09 — ANSI quotes + validation hardening

### 4.2f — `ANSI_QUOTES` OFF by default

- new sessions now parse `"..."` as string literals by default, matching MySQL
- `SET sql_mode = 'ANSI_QUOTES'` switches double quotes to identifier mode for
  subsequent statements in the same session
- prepared statements, plan-cache normalization, and multi-statement packet
  splitting all reuse the same quote-mode bit

### Validation fixes folded into the closeout

- `FOUND_ROWS()` now captures the pre-limit row count even on join/ctx paths
- clustered insert staging rolls back only the failing statement instead of
  discarding earlier staged rows from the same transaction
- clustered partial-index maintenance now treats predicate-only changes as
  index-affecting and ignores stale dead secondary entries during uniqueness checks
- connection cleanup (`COM_RESET_CONNECTION`, `COM_CHANGE_USER`, disconnect)
  now rolls back any still-open session transaction before releasing the session
- local macOS build wrappers were added under `.cargo/` and `tools/` so tests
  keep running around the current `com.apple.provenance` issue
