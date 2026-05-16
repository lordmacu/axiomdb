# Phase 20 — Types + import/export

## 20.1 — Regular views (2026-04-27)

### What was built

**Regular SQL views** — `CREATE VIEW`, `CREATE OR REPLACE VIEW`, `DROP VIEW [IF EXISTS]`, `SHOW CREATE VIEW`, and transparent read-only access through views via `SELECT`.

### Architecture

#### Catalog layer (`axiomdb-catalog`)

- `RelationKind::View` (tag = 2) added to the existing enum alongside `Table` and `MaterializedView`.
- `TableDef::is_view()` helper.
- `CatalogWriter::create_view(schema, name, defining_query)` — allocates a `TableDef` with `root_page_id = 0` (no physical pages) and `RelationKind::View`; stores the raw SQL text in `defining_query`.
- `CatalogWriter::replace_view_query(table_id, new_query)` — used by `CREATE OR REPLACE VIEW` to update the stored SQL without changing the catalog entry ID.
- `CatalogReader` already reads all `TableDef` rows generically, so no changes were needed there.

#### Parser / AST (`axiomdb-sql`)

New AST nodes in `ast.rs`:
- `CreateViewStmt { or_replace, view, columns, query_sql, select }` — `query_sql` is the raw SQL text stored in the catalog.
- `DropViewStmt { if_exists, views }` — supports multi-name `DROP VIEW v1, v2, v3`.
- `ShowCreateViewStmt { view }`

Parser (`parser/ddl.rs`):
- `parse_create_view` — parses `CREATE [OR REPLACE] VIEW name [(col, ...)] AS SELECT ...`, captures the raw SQL text from the token stream.
- `parse_drop_view`, `parse_show_create_view`.

#### View expansion (transparent reads)

The key design is **analysis-time expansion** — views are rewritten into subqueries before any execution, matching the existing CTE expansion pattern.

In `analyzer_stmt.rs`:
- `expand_views(stmt, ...)` — called on every `SelectStmt` before `expand_ctes()`.
- `substitute_view_ref(from, ...)` — when a `FromClause::Table(tref)` resolves to a `RelationKind::View` in the catalog, it:
  1. Checks for circular view references via a `HashSet<String>` called `expanding`.
  2. Re-parses the stored `defining_query` SQL text.
  3. Recursively calls `expand_views()` on the parsed inner select to handle nested views.
  4. Returns `FromClause::Subquery { query: inner_select, alias, lateral: false }`.

This means the executor never sees view references — they have been transparently rewritten into subqueries.

#### Executor changes

`executor/ddl_view.rs` (new file, included via `include!` into `mod.rs`):
- `execute_create_view` — validates existence, calls `CatalogWriter::create_view` or `replace_view_query`.
- `execute_show_create_view` — reads `TableDef`, reconstructs `CREATE VIEW \`name\` AS <query>`.
- `execute_drop_view` — multi-name; respects `IF EXISTS`; checks `is_view()` to reject base tables.

`executor/select_core.rs`:
- `execute_select_derived` extended: after materializing the inner subquery rows, if `stmt.joins` is non-empty, routes to `execute_select_with_joins_first_materialized` — the same shared join-loop entry point used by `JSON_TABLE`. This fixes the `FromClause::Subquery + JOIN` path that was previously unhandled.

`executor/exec_entry.rs` (read-only path):
- Added `Stmt::ShowCreateView` case — `SHOW CREATE VIEW` starts with "SHOW" so it goes through the read-only executor path, which now handles it.

`executor/exec_dispatch.rs`:
- Dispatches `Stmt::CreateView`, `Stmt::DropView`, `Stmt::ShowCreateView`.

#### SHOW TABLES and information_schema

- `show_table_type_name()` updated to return `"VIEW"` when `table.is_view()`.
- `information_schema.VIEWS` new virtual table:
  - Columns: `TABLE_CATALOG`, `TABLE_SCHEMA`, `TABLE_NAME`, `VIEW_DEFINITION`, `CHECK_OPTION` (always `NONE`), `IS_UPDATABLE` (always `NO`).
  - `generate_is_views_rows()` filters catalog tables by `is_view()`.
- `is_table_cols("views")` added to `information_schema.rs`.
- `information_schema.TABLES` already reports `TABLE_TYPE = 'VIEW'` via the updated `show_table_type_name()` function.

### Coverage

- `crates/axiomdb-sql/tests/integration_views.rs` — 16 integration tests:
  - CREATE VIEW persists catalog entry
  - Duplicate view error
  - CREATE OR REPLACE VIEW updates definition
  - CREATE VIEW on existing table returns error
  - DROP VIEW removes catalog entry
  - DROP VIEW IF EXISTS on missing view succeeds
  - DROP VIEW on missing view returns error
  - DROP VIEW on base table returns error
  - DROP VIEW multi-name
  - SELECT from view expands transparently
  - View in JOIN (resolved via subquery expansion)
  - Nested view expansion (view on view)
  - Circular view reference error
  - SHOW CREATE VIEW returns DDL
  - SHOW CREATE VIEW on table returns error
  - information_schema.VIEWS returns view rows
- Wire smoke block `[20.1 regular views]` in `tools/wire-test.py` — 8 checks (473/473 total).

### Deferred to later phases

- Updatable views (INSERT/UPDATE/DELETE through a view) — requires write-path integration.
- `WITH CHECK OPTION` — requires updatable views.
- Column-name alias list (`CREATE VIEW v (a, b, c) AS SELECT ...`) — parser accepts it; executor stores but does not remap column names at query time.
- Security-definer/invoker views.
- `SHOW FULL TABLES WHERE Table_type = 'VIEW'` filtering.

## 20.2 — Sequences (2026-04-29)

### What was built

Standalone SQL sequences: `CREATE SEQUENCE`, `DROP SEQUENCE`, `NEXTVAL(text)`,
and `CURRVAL(text)`.

### Architecture

- `SequenceDef` stores schema/name plus `last_value`, `start_value`,
  `increment`, `min_value`, `max_value`, `cycle`, `cache_size`, and `is_called`.
- `axiom_sequences` is a new catalog heap root stored in the meta page and
  lazily initialized for legacy databases.
- `NEXTVAL` advances sequence state through a short internal transaction that
  commits immediately, so user rollback does not reuse consumed values.
- `CURRVAL` is held in `SessionContext.sequence_currvals`, keyed by lowercase
  `schema.sequence`.
- `SELECT` without `FROM` now uses the real session context in ctx execution so
  session functions like `CURRVAL` see state created by previous statements.

### Coverage

- `crates/axiomdb-sql/tests/integration_sequences.rs` — 12 integration tests:
  create/drop, `IF EXISTS`, duplicate create, invalid options, `NEXTVAL`,
  per-output-row advancement, `CURRVAL`, rollback gaps, and exhaustion.
- `crates/axiomdb-sql/tests/integration_ddl_parser.rs` — sequence parser tests.
- `tools/wire-test.py` — block `[20.2 sequences]` (476/476 total).
- Closeout gates: `cargo test --workspace`, `cargo clippy --workspace -- -D warnings`,
  and `cargo fmt --check`.

### Deferred to later phases

- `ALTER SEQUENCE`, `SETVAL`, `OWNED BY`, and sequence privileges.
- Wiring `SERIAL` / identity columns to standalone sequence objects.
- Sequence cache preallocation beyond `CACHE 1`.

## 20.3 — ENUM types (2026-05-12)

### What was built

User-defined ENUM types: `CREATE TYPE name AS ENUM (...)`, `DROP TYPE`, ENUM
column DDL, validated INSERT, SELECT, and WHERE filtering.

### Architecture

#### Type system (`axiomdb-types`)

- `DataType::UserDefinedEnum(type_name: String)` — carries the qualified type name.
- `Value::Text` is reused at runtime; ENUM values are stored as plain strings.

#### Catalog layer (`axiomdb-catalog`)

- `EnumTypeDef { id, schema, name, variants }` — on-disk entry in the `axiom_enum_types` catalog heap.
- `CatalogWriter::create_enum_type` / `delete_enum_type`.
- `CatalogReader::list_enum_types`, `get_enum_type_by_name`.
- `ColumnDef.enum_type_name: Option<String>` — stores the qualified enum type reference.
- **Binary format fix (Phase 20.4 audit)**: when `collation=None` and `enum_type_name=Some(...)`,
  the encoder now always writes an empty collation section so the decoder can unambiguously find
  the enum field (prevents silent data corruption).

#### Parser / Executor

- `CREATE TYPE … AS ENUM` and `DROP TYPE` AST nodes.
- Column definitions accept `col_name type_name` where `type_name` resolves to an ENUM.
- `INSERT` validates each ENUM value against the stored variant list.

### Coverage

- `crates/axiomdb-sql/tests/integration_enums.rs` — integration tests covering
  DDL, persistence, validation, and SELECT/WHERE.
- Wire smoke block `[20.3 enum types]` in `tools/wire-test.py`.

### Deferred to later phases

- `ALTER TYPE … ADD VALUE`, `RENAME VALUE`.
- `ORDER BY` on ENUM columns using variant-list order rather than lexicographic.
- ENUM in information_schema.COLUMNS reporting.

## 20.4 — SQL Arrays (2026-05-13)

### What was built

Full PostgreSQL-compatible SQL arrays: column DDL (`INT[]`, `TEXT[][]`, …),
array literals (`ARRAY[…]`), subscript (`a[1]`), slice (`a[1:3]`), operators
(`@>`, `<@`, `&&`, `||`, `=`, `<>`), functions (`array_length`, `array_ndims`,
`cardinality`, `array_append`, `array_prepend`, `array_cat`, `array_remove`,
`array_replace`, `array_upper`, `array_lower`, `array_fill`, `array_to_string`,
`string_to_array`, `array_position`, `array_positions`), `unnest()` set-returning
function, `array_agg()` aggregate, and GIN index support.

### Architecture

#### Type system (`axiomdb-types`)

- `Value::Array(ArrayValue)` — recursive typed array: carries element type, dimensions,
  and a flat `Vec<Value>` payload.
- `ArrayValue` serializes to/from bytes via `array_codec.rs` using a PostgreSQL-compatible
  binary format: header (element type, ndims, flags) + dimension descriptors (len, lower bound)
  + flat element payload.
- TOAST threshold: individual arrays larger than 8 KB are stored via the TOAST mechanism
  when they exceed the row-level threshold; the array codec itself handles up to 16 MB.

#### Catalog layer (`axiomdb-catalog`)

- `ColumnDef.array_ndims: Option<u8>` — stored as trailing byte after collation/enum fields.
- `ColumnDef.array_element_type: Option<ColumnType>` — element type stored as trailing byte.
- `ColumnType::Array` tag registered.

#### Parser / AST (`axiomdb-sql`)

- `Expr::ArrayLiteral(Vec<Expr>)` — `ARRAY[e1, e2, …]`.
- `Expr::ArraySubscript { expr, index }` — `a[i]`.
- `Expr::ArraySlice { expr, lo, hi }` — `a[lo:hi]`.
- `Expr::ArrayOp { left, op: ArrayOp, right }` — `@>`, `<@`, `&&`, `||`, `=`, `<>`.
- `ANY(array)` / `ALL(array)` — parsed and evaluated.

#### Executor

- `array_io.rs` — evaluates all array expressions and functions.
- `array_agg` aggregate — collects values across groups.
- `unnest()` handled as a set-returning function in the FROM clause.
- GIN index integration: array `@>` and `&&` operators can use a GIN index when
  one exists on the array column (reusing Phase 11.17 GIN infrastructure).

#### Binary serialization invariant

Trailing fields in `ColumnDef` binary format are written in a fixed order with
an explicit collation section always present when `enum_type_name` is set. This
ensures the decoder always finds enum bytes at the correct offset without ambiguity.

### Coverage

- `crates/axiomdb-sql/tests/integration_array_operators.rs` — subscript, slice, operators.
- `crates/axiomdb-sql/tests/integration_array_functions.rs` — all built-in array functions.
- `crates/axiomdb-sql/tests/integration_array_unnest.rs` — `unnest()` set-returning function.
- `crates/axiomdb-sql/tests/integration_array_any_all.rs` — `ANY`/`ALL` operator semantics.
- `crates/axiomdb-catalog/tests/…` — `ColumnDef` round-trip for array columns.
- Wire smoke block `[20.4 sql arrays]` in `tools/wire-test.py`.

### Deferred to later phases

- Multi-dimensional `unnest()` with multiple array arguments (zip semantics).
- `array_dims()` returning text format like `[1:3][1:2]`.
- `ARRAY(SELECT …)` subquery-to-array constructor.
- Updatable array subscript assignment (`UPDATE t SET a[1] = 42`).

## 20.7 — Incremental backup (2026-05-15)

### What was built

**BACKUP DATABASE TO** and **RESTORE DATABASE FROM** SQL commands with full and incremental backup modes backed by a custom `.axbk` binary format.

### SQL syntax

```sql
-- Full backup
BACKUP DATABASE TO '/path/to/backup.axbk';

-- Incremental (page-diff from a previous full backup)
BACKUP DATABASE TO '/path/inc.axbk' INCREMENTAL FROM '/path/full.axbk';

-- Restore (full or incremental automatically resolved)
RESTORE DATABASE FROM '/path/backup.axbk' TO '/path/restore.db';
```

### Binary format (`.axbk`)

```
Offset  Size  Field
0       8     magic = 0x4158494F4D424B01 ("AXIOMBK\x01")
8       1     kind  = 0 (Full) | 1 (Incremental)
9       7     _pad
16      8     backup_lsn   (checkpoint LSN at backup time)
24      8     page_count   (storage.page_count() at backup time)
32      4     page_size    (always PAGE_SIZE = 16384)
36      4     _pad2
40      8     base_lsn     (Full: 0; Incremental: base backup_lsn)
48      8     delta_count  (Full: page_count; Incremental: # changed pages)
56      72    base_path    (NUL-terminated UTF-8; max 71 chars + NUL)
128+    ...   page entries: { page_id: u64, page_bytes: [u8; PAGE_SIZE] }
```

### Architecture

#### StorageEngine trait (`axiomdb-storage`)

- `read_page_raw(&self, page_id: u64) -> Result<[u8; PAGE_SIZE], DbError>` added to `StorageEngine` trait — reads raw page bytes without checksum validation.
  - `MmapStorage`: delegates to existing `copy_raw_page_from_mmap`.
  - `MemoryStorage`: copies from the flat page array.
  - All test implementors (`SharedMemoryStorage`, `CountingStorage`, `CountingPrefetchStorage`) delegate to their inner `MemoryStorage`.
- Raw reads are semantically correct for backup paths where all pages must be captured regardless of checksum state (e.g., during an ongoing write epoch on macOS where mmap and pwrite can show different checksum bytes).

#### Parser / AST (`axiomdb-sql`)

- `BackupStmt { dest: String, incremental_from: Option<String> }` — AST node.
- `RestoreStmt { source: String, dest_path: String }` — AST node.
- `parse_backup` / `parse_restore` in `parser/dml.rs` handle `TOKEN::Database` (reserved keyword, not ident).
- `Stmt::Backup` / `Stmt::Restore` wired through all 14 match sites (dispatch, explain, analyzer no-op, etc.).

#### Executor (`axiomdb-sql/src/executor/backup.rs`)

Included directly into `executor/mod.rs` via `include!()`.

- `execute_backup` — checks destination doesn't exist, dispatches to `backup_full` or `backup_incremental`.
- `backup_full` — checkpoints via `txn.checkpoint(storage)`, streams all pages with 64-page prefetch hints.
- `backup_incremental` — validates base backup is Full kind, builds `HashMap<page_id → checksum>` by scanning the base file, checkpoints, reads all current pages and diffs CRC32c (stored at header offset 12 in each page), writes only changed pages.
- `restore_from_source` — auto-detects kind; Full: streams all pages directly; Incremental: restores base first via `write_pages_to_dest`, then applies delta on top.
- `write_pages_to_dest` — uses `write_at` (POSIX `pwrite64`) for sparse writes; syncs with `sync_all` after all pages written.

#### Dispatcher (`executor/exec_with_ctx.rs`)

BACKUP/RESTORE intercepted at the **top** of `execute_with_ctx_locked` before all transaction-wrapping logic, mirroring the CHECKPOINT pattern. This avoids the engine from trying to wrap a self-checkpointing operation inside an active transaction.

### Coverage

- `crates/axiomdb-sql/tests/integration_backup_parser.rs` — 8 parser tests (full/incremental syntax, case-insensitive, error cases).
- Wire smoke block `[20.7 backup/restore]` in `tools/wire-test.py` — 8 assertions (562/562).

### Deferred to later phases

- Chains of 3+ incremental backups (incremental-from-incremental).
- Streaming backup over a network socket without writing a temp file.
- `base_path` > 71 bytes (documented workaround: use a symlink to shorten the path).
- Backup encryption / compression wrappers.

## 20.8 — COPY FROM streaming (2026-05-15)

### What was built

`COPY table FROM 'path'` now processes CSV and JSONL files in O(batch_size) memory
regardless of file size. A 10 GB CSV with 200 M rows no longer requires 200 GB RSS
before the first insert.

### Architecture

#### Constant

```rust
const COPY_BATCH_SIZE: usize = 1024;
```

#### CSV streaming (`stream_copy_csv`)

The `csv::Reader` already iterates records lazily. The change is that `stream_copy_csv`
no longer collects into `Vec<Vec<Value>>` — instead it accumulates up to `COPY_BATCH_SIZE`
rows, then calls `flush_batch` → `execute_insert_ctx`. Repeat until EOF; flush any partial
final batch.

Memory: at most `COPY_BATCH_SIZE × cols × ~64 B` ≈ 320 KB per batch for a 5-column table.

#### JSONL schema-first streaming (`stream_copy_jsonl`)

The old JSONL implementation did two passes: collect all lines to discover column names,
then emit rows. The new implementation:

1. Calls `resolve_table_cached` once to get the table's column list.
2. Builds `col_index: HashMap<String, usize>` from the schema.
3. Iterates `reader.lines()`, parses each non-empty line as a JSON object, maps keys
   through `col_index` into a fixed-width `row = vec![Value::Null; col_count]`.
4. Unknown keys → silently ignored. Missing keys → `Value::Null`.
5. Batches of 1024 flushed via `flush_batch`.

#### JSON array

`parse_json_file` unchanged. JSON arrays (`[{...}]`) require full load.
Users should use JSONL format for files that exceed available RAM.

#### Transaction semantics

All batches share the same `ConnectionTxn`. A parse error or FK violation on any batch
returns `Err` to the caller, which rolls back all previously inserted batches. Atomicity
is preserved — identical to PostgreSQL COPY FROM behavior.

### Coverage

- `crates/axiomdb-sql/tests/integration_copy.rs` — 5 new streaming tests.
- Wire smoke block `[20.8 COPY streaming]` in `tools/wire-test.py` — 2 assertions (564/564).

### Deferred to later phases

- JSONL: strict unknown-column error mode (opt-in flag).
- JSON array streaming via `serde_json::StreamDeserializer`.
- Configurable `COPY_BATCH_SIZE` via WITH options.

---

## Subphase 20.16 — Business Calendar Functions

**Closed:** 2026-05-15

### What was built

Holiday calendar management DDL and three scalar business-day functions.

### SQL surface

```sql
-- DDL
CREATE HOLIDAY CALENDAR 'CO' WITH HOLIDAYS ('2024-01-01', '2024-07-04');
CREATE HOLIDAY CALENDAR 'US';            -- empty calendar
DROP HOLIDAY CALENDAR 'CO';
DROP HOLIDAY CALENDAR IF EXISTS 'CO';   -- idempotent

-- Scalar functions
SELECT IS_BUSINESS_DAY('2024-01-01', 'CO');          -- 0 (holiday)
SELECT IS_BUSINESS_DAY('2024-01-02', 'CO');          -- 1 (Tuesday, no holiday)
SELECT IS_BUSINESS_DAY('2024-01-06', 'CO');          -- 0 (Saturday)

SELECT NEXT_BUSINESS_DAY('2024-01-05', 'CO');        -- 2024-01-08 (Monday) as days-since-epoch
SELECT BUSINESS_DAYS_BETWEEN('2024-01-01', '2024-01-08', 'CO');  -- 4 (if 01-01 is holiday)
```

### Architecture

#### Catalog layer

New constant in storage meta: `CATALOG_HOLIDAY_CALENDARS_ROOT_BODY_OFFSET = 184`.
`HolidayCalendarDef` serialized as:

```
[code_len: u8][country_code: UTF-8][count: u16 LE][i32 LE × count]
```

Holidays stored sorted ascending, deduplicated. Country code always uppercase.

`CatalogBootstrap::ensure_holiday_calendars_root` — lazy allocation on first `CREATE`.

#### Session cache

`SessionContext.holiday_cache: HashMap<String, Arc<HashSet<i32>>>` — loaded from catalog
on first call per country code, cleared by `invalidate_all()` on any DDL statement.

#### is_weekday formula

```rust
fn is_weekday(day: i32) -> bool { ((day + 3).rem_euclid(7) as u32) < 5 }
```

Mapping: `(day + 3) % 7` → 0=Mon … 4=Fri, 5=Sat, 6=Sun.
Epoch 1970-01-01 is Thursday → `(0 + 3) % 7 = 3` ✓.

#### BUSINESS_DAYS_BETWEEN O(1) formula

```
span = end - start (days)
full_weeks = span / 7
remainder = span % 7
start_dow = (start + 3) % 7   -- 0=Mon…6=Sun
remainder_weekdays = count of i in [0, remainder) where (start_dow + i) % 7 < 5
total_weekdays = full_weeks * 5 + remainder_weekdays
result = total_weekdays - holidays_in_range
```

O(1) for weekday count + O(|holidays|) for subtraction.

### Coverage

- `crates/axiomdb-catalog/src/schema_holiday_calendar.rs` — 4 unit tests
- `crates/axiomdb-sql/tests/integration_business_calendar.rs` — 21 integration tests
- `tools/wire-test.py` — 4 wire assertions: `[20.16a..d]`
- All 2829 axiomdb-sql tests pass; clippy + fmt clean.
