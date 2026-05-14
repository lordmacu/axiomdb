# Catalog System

The catalog is AxiomDB's schema repository. It stores the definition of logical
databases, tables, columns, indexes, constraints, foreign keys, and planner
statistics, then makes that information available to the SQL analyzer and
executor through a consistent, MVCC-aware reader interface.

---

## Design Goals

- **Self-describing:** The catalog tables are themselves stored as regular heap pages.
  The engine needs no external schema file.
- **Persistent:** Catalog data survives crashes. The WAL treats catalog mutations like
  any other transaction.
- **MVCC-visible:** A DDL statement that creates a table is visible to subsequent
  statements in the same transaction but invisible to concurrent transactions until
  committed.
- **Bootstrappable:** An empty database file contains no catalog rows. The first
  `open()` runs a special bootstrap path that allocates the catalog roots and inserts
  the default logical database `axiomdb`.

---

## System Tables

The catalog consists of eight logical heaps rooted from the meta page. User-facing
introspection is documented in [Catalog & Schema](../user-guide/features/catalog.md).

| Table                   | Meta offset | Contents                                         |
|-------------------------|-------------|--------------------------------------------------|
| `axiom_tables`          | 32          | One row per user-visible table                   |
| `axiom_columns`         | 40          | One row per column, in declaration order         |
| `axiom_indexes`         | 48          | One row per index (includes partial index predicate since Phase 6.7) |
| `axiom_constraints`     | 72          | Named CHECK constraints (Phase 4.22b)            |
| `axiom_foreign_keys`    | 84          | One row per FK constraint (Phase 6.5)            |
| `axiom_stats`           | 96          | Per-column NDV and row_count for planner (Phase 6.10) |
| `axiom_databases`       | 104         | One row per logical database                     |
| `axiom_table_databases` | 112         | Optional table ownership binding by database     |

Each root page is stored at the corresponding u64 body offset in the meta page
(page 0). Older database files may have `0` in the new database offsets; the
open path upgrades them lazily by allocating the roots and inserting
`axiomdb`.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Separate DB Ownership</span>
AxiomDB deliberately does <strong>not</strong> overload <code>schema_name</code> inside
<code>TableDef</code> to fake a database namespace. Keeping database ownership in
<code>axiom_table_databases</code> preserves on-disk compatibility now and leaves
real <code>CREATE SCHEMA</code> room later, unlike a shortcut that would collapse two
separate namespaces into one field.
</div>
</div>

### `axiom_databases` row format (`DatabaseDef`)

```text
[name_len: 1 byte u8]
[name:     name_len UTF-8 bytes]
```

Fresh databases always contain:

```text
axiomdb
```

### `axiom_table_databases` row format (`TableDatabaseDef`)

```text
[table_id:        4 bytes LE u32]
[name_len:        1 byte  u8]
[database_name:   name_len UTF-8 bytes]
```

Missing binding row means: this is a legacy table owned by `axiomdb`.

### `axiom_stats` row format (`StatsDef`)

```text
[table_id:  4 bytes LE u32]
[col_idx:   2 bytes LE u16]
[row_count: 8 bytes LE u64]  — visible rows at last ANALYZE / CREATE INDEX
[ndv:       8 bytes LE i64]  — distinct non-NULL values (PostgreSQL stadistinct encoding)
```

`ndv` encoding (same as PostgreSQL `stadistinct`):
- `> 0` → absolute count (e.g. 9999 unique emails)
- `= 0` → unknown → planner uses `DEFAULT_NUM_DISTINCT = 200`

Stats root is **lazily initialized** at first write (`ensure_stats_root`). Pre-6.10
databases open without migration: `list_stats` returns empty vec when root = 0,
causing the planner to use the conservative default (always use index).

Stats are bootstrapped at `CREATE INDEX` time by reusing the table scan already
performed for B-Tree build — no extra I/O. `ANALYZE TABLE` refreshes them with
an exact full-table NDV count.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Exact NDV, Not Sampling</span>
AxiomDB computes exact distinct value counts using a HashSet of encoded key bytes.
PostgreSQL uses Vitter's reservoir sampling algorithm (Duj1 estimator) for large
tables to avoid the O(n) full scan. Exact counting is correct and simpler for the
typical table sizes of an embedded database. Sampling is planned for a future
statistics phase when tables exceed 1 M rows.
</div>
</div>

### `axiom_foreign_keys` row format (`FkDef`)

```text
[fk_id:          4 bytes LE u32]
[child_table_id: 4 bytes LE u32]   — table with the FK column
[child_col_idx:  2 bytes LE u16]   — FK column index in child table
[parent_table_id:4 bytes LE u32]   — referenced (parent) table
[parent_col_idx: 2 bytes LE u16]   — referenced column in parent table
[on_delete:      1 byte  u8   ]    — 0=NoAction, 1=Restrict, 2=Cascade, 3=SetNull
[on_update:      1 byte  u8   ]    — same encoding
[fk_index_id:    4 bytes LE u32]   — 0 = user-provided index (not auto-created)
[name_len:       4 bytes LE u32]
[name:           name_len bytes UTF-8]
```

`FkAction` encoding: `0` = NoAction, `1` = Restrict, `2` = Cascade,
`3` = SetNull, `4` = SetDefault.

`fk_index_id = 0` means the FK column already had a user-provided index; the FK
did not auto-create one and will not drop one on `DROP CONSTRAINT`.

### `axiom_indexes` — predicate extension (Phase 6.7)

The `IndexDef` binary format was extended in Phase 6.7 with a backward-compatible
predicate section appended after the columns:

```text
[...existing fields...][ncols:1][col_idx:2, order:1]×ncols
[pred_len:2 LE][pred_sql: pred_len UTF-8 bytes]   ← absent on pre-6.7 rows
```

`pred_len = 0` (or section absent) → full index. Pre-6.7 databases open without
migration because `from_bytes` checks `bytes.len() > consumed` before reading
the predicate section.

---

## CatalogBootstrap

`CatalogBootstrap` is a one-time procedure that runs when `open()` encounters an
empty database file (or a file with the meta page uninitialized).

### Bootstrap Sequence

```
1. Allocate page 0 (Meta page).
   Write format_version, zero for catalog_root_page, freelist_root_page, etc.

2. Allocate the freelist root page.
   Initialize the bitmap (all pages allocated so far are marked used).
   Write freelist_root_page into the meta page.

3. Allocate heap roots for catalog tables and aux heaps:
   `axiom_tables`, `axiom_columns`, `axiom_indexes`, `axiom_constraints`,
   `axiom_foreign_keys`, `axiom_stats`, `axiom_databases`, `axiom_table_databases`.

4. Insert the default database row `axiomdb` into `axiom_databases`.

5. Persist every root page id into the meta page.

6. Flush pages and WAL.
```

Fresh bootstrap uses `txn_id = 0` for the default database row because no user
transaction exists yet. If a pre-22b.3a database is reopened, `ensure_database_roots`
upgrades it in-place and inserts `axiomdb` exactly once.

---

## CatalogReader

`CatalogReader` provides read-only access to the catalog from any component that
needs schema information (primarily the SQL analyzer).

```rust
pub struct CatalogReader<'a> {
    storage:  &'a dyn StorageEngine,
    snapshot: TransactionSnapshot,
}

impl<'a> CatalogReader<'a> {
    /// List all user tables visible to this snapshot.
    pub fn list_tables(&mut self, schema: &str) -> Result<Vec<TableDef>, DbError>;

    /// List all logical databases visible to this snapshot.
    pub fn list_databases(&mut self) -> Result<Vec<DatabaseDef>, DbError>;

    /// Find a specific table by schema + name.
    pub fn get_table(&mut self, schema: &str, name: &str) -> Result<Option<TableDef>, DbError>;

    /// Find a specific table by database + schema + name.
    pub fn get_table_in_database(
        &mut self,
        database: &str,
        schema: &str,
        name: &str,
    ) -> Result<Option<TableDef>, DbError>;

    /// List columns for a table in declaration order.
    pub fn list_columns(&mut self, table_id: u64) -> Result<Vec<ColumnDef>, DbError>;

    /// List indexes for a table.
    pub fn list_indexes(&mut self, table_id: u64) -> Result<Vec<IndexDef>, DbError>;
}
```

The `snapshot` parameter ensures catalog reads are MVCC-consistent. A DDL statement
that has not yet committed is invisible to other transactions' `CatalogReader`.

### Effective database resolution

Catalog lookup is now two-dimensional:

```text
(database, schema, table)
```

The resolver applies one legacy rule:

```text
if no explicit table->database binding exists:
    effective database = "axiomdb"
```

That rule is what lets old databases keep working without rewriting existing
`TableDef` rows.

---

## Schema Types

```rust
pub struct TableDef {
    pub id:             u32,
    pub root_page_id:   u64,    // heap root or clustered-tree root
    pub storage_layout: TableStorageLayout,
    pub schema_name:    String,
    pub table_name:     String,
    pub schema_version: u64,    // monotonic counter for plan cache invalidation (Phase 40.2)
}

pub enum TableStorageLayout {
    Heap = 0,
    Clustered = 1,
}

// On-disk format for axiom_tables rows (3 generations, all backward-compatible):
//
// v0 (legacy, no trailing bytes):
//   [table_id:4 LE][root_page_id:8 LE][schema_len:1][schema UTF-8][name_len:1][name UTF-8]
//   → storage_layout = Heap, schema_version = 1
//
// v1 (1 trailing byte):
//   ... [layout:1]
//   → layout from byte, schema_version = 1
//
// v2 (9 trailing bytes, current):
//   ... [layout:1][schema_version:8 LE]
//   → layout and schema_version from bytes
//
// `schema_version` is initialized to 1 at table creation. It is bumped by:
// CREATE INDEX, DROP INDEX, ALTER TABLE (any op), DROP TABLE, TRUNCATE TABLE.
// Plans whose deps include (table_id, old_version) detect staleness on next
// lookup without scanning the entire plan cache (Phase 40.2 OID invalidation).

pub struct ColumnDef {
    pub table_id:         u64,
    pub col_idx:          usize,
    pub name:             String,
    pub col_type:         ColumnType,
    pub nullable:         bool,
    pub auto_increment:   bool,
    pub type_len:         u16,
    pub is_fixed_len:     bool,
    pub default_expr:     Option<String>,
    pub on_update_expr:   Option<String>,
    pub generated_expr:   Option<String>,
    pub generated_stored: bool,
}

pub struct IndexDef {
    pub id:           u64,
    pub table_id:     u64,
    pub index_name:   String,
    pub is_unique:    bool,
    pub is_primary:   bool,
    pub columns:      Vec<String>,  // indexed column names in key order
    pub root_page_id: u64,          // B+ Tree root, or clustered table root for PRIMARY KEY metadata
}
```

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Generic Table Roots</span>
`TableDef` no longer hard-codes a heap root because Phase 39.13 makes explicit-`PRIMARY KEY` tables clustered from day one. This follows SQLite `WITHOUT ROWID` more closely than the easier InnoDB-style hidden-key shortcut, which would have preserved the old heap assumption at the cost of reopening the storage rewrite later.
</div>
</div>

### Generated-column persistence (Phase 21.5f)

`axiom_columns` gained two backward-compatible extensions:

- `flags bit6` -> generated expression bytes are present after
  `on_update_expr`
- `flags bit7` -> generated kind (`0 = STORED`, `1 = VIRTUAL`)

When bit6 is set, the row appends:

```text
[generated_expr_len: 2 bytes LE][generated_expr utf8 bytes]
```

Old rows keep bit6 clear and decode exactly as before. The executor reparses the
stored SQL text at write time so heap INSERT, clustered INSERT, UPDATE, ODKU,
`ON CONFLICT`, and `MERGE` all reuse one materialization rule.

---

## DDL Mutations Through the Catalog

When the executor processes `CREATE TABLE`, it:

1. Opens a write transaction (or participates in the current one).
2. Allocates a new `TableId` from the meta page sequence.
3. Chooses the table layout:
   - no explicit `PRIMARY KEY` → `Heap`
   - explicit `PRIMARY KEY` → `Clustered`
4. Allocates the primary row-store root page:
   - `Heap` → `PageType::Data`
   - `Clustered` → `PageType::ClusteredLeaf`
5. Inserts a row into `axiom_tables` with `{id, root_page_id, storage_layout, schema_name, table_name}`.
6. Inserts one row per column into `axiom_columns`.
7. Persists index metadata:
   - clustered tables reuse `table.root_page_id` for the logical PRIMARY KEY index row
   - `UNIQUE` secondary indexes still allocate ordinary `PageType::Index` roots
8. Appends all these mutations to the WAL.
9. Commits (or defers the commit to the surrounding transaction).

The `root_page_id` stored in `axiom_tables` is now the single entry point for the
table's primary row store. Heap DML still uses it as the heap-chain root today;
clustered `INSERT` / `SELECT` now use it as the clustered row-store root, while
heap-only executor paths still reject clustered `UPDATE` / `DELETE` instead of
touching the wrong page format.

Because the catalog is stored in heap pages and indexed like any other table, all
crash recovery mechanisms apply automatically: WAL replay will reconstruct the catalog
state after a crash in the middle of `CREATE TABLE`, just as it would reconstruct
any other table mutation.

---

## Catalog Page Organization

```
Page 0:      Meta page (format_version, catalog_root_page, freelist_root_page, ...)
Page 1:      FreeList bitmap root
Pages 2–N:   B+ Tree pages for axiom_tables
Pages N+1–M: Heap pages for axiom_tables row data
Pages M+1–P: B+ Tree pages for axiom_columns
...
Pages P+1–Q: User table data begins here
```

The exact page assignments depend on database growth. Page 0 always remains the meta
page. All other page assignments are dynamic — the freelist tracks which pages are
in use, and the meta page records the root page IDs for each catalog B+ Tree.

---

## Catalog Invariants

The following invariants must hold at all times. The startup verifier in
`axiomdb-sql::index_integrity` now re-checks the index-related ones after WAL
recovery and before server or embedded mode starts serving traffic:

1. Every table listed in `axiom_tables` has at least one row in `axiom_columns`.
2. Every column in `axiom_columns` references a `table_id` that exists in `axiom_tables`.
3. Every index in `axiom_indexes` references a `table_id` that exists in `axiom_tables`.
4. Every non-clustered `root_page_id` in `axiom_indexes` points to a page of type `Index`.
5. A clustered table's PRIMARY KEY metadata row in `axiom_indexes` reuses the table `root_page_id` and therefore may point to `ClusteredLeaf` / `ClusteredInternal`.
6. Every column listed in an index definition exists in the referenced table.
7. No two tables in the same schema have the same name.
8. No two indexes on the same table have the same name.

### Startup index integrity verification

For every catalog-visible heap table:

1. enumerate the expected entries from heap-visible rows
2. enumerate the actual B+ Tree entries from `root_page_id`
3. compare them exactly
4. if the tree is readable but divergent, rebuild a fresh root from heap
5. rotate the catalog root in a WAL-protected transaction
6. defer free of the old tree pages until commit durability is confirmed

Clustered tables are skipped for now because their logical PRIMARY KEY metadata
no longer points at a classic B+ Tree root. If a heap-side tree cannot be
traversed safely, open fails with `IndexIntegrityFailure`. The database does
**not** enter a best-effort serving mode with an untrusted index.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Heap As Source Of Truth</span>
Like SQLite's <code>REINDEX</code>, AxiomDB rebuilds a readable divergent index from heap rows
instead of trying to patch arbitrary leaf-level damage in place. This keeps recovery logic small
and makes the catalog root swap the only logical state transition.
</div>
</div>

---

## Custom Aggregate Catalog (Phase 13.14)

Custom aggregates are first-class catalog objects stored alongside tables,
indexes, and views.

### `AggregateDef` — on-disk format

```
[schema_len: u8][schema: bytes]
[name_len:   u8][name:   bytes]
[arg_count:  u8][arg_types: u8 * arg_count]
[sfunc_len:  u8][sfunc:  bytes]
[stype_len:  u8][stype:  bytes]
[finalfunc_len: u8][finalfunc: bytes]   -- empty string if None
[helper_kind: u8]                        -- AggregateHelperKind tag
```

All length prefixes are single-byte so maximum field length is 255 bytes.
`helper_kind` is a stable numeric tag (`1 = Median`) that survives
serialization and lets the executor skip re-validating helper names at query
time.

### Catalog APIs

| Function | Description |
|---|---|
| `CatalogWriter::create_aggregate(def)` | Persists a new `AggregateDef` |
| `CatalogReader::get_aggregate(schema, name, arity)` | Looks up by name + arg count |
| `CatalogWriter::delete_aggregate(schema, name, arity)` | Removes the entry |

### Registry boundary

`SFUNC` / `FINALFUNC` names are validated at `CREATE AGGREGATE` time against
`custom_aggregate::resolve_custom_aggregate_helper`. Only combinations present
in the registry are accepted. This keeps the executor simple: at runtime the
only decision is which `AggregateHelperKind` accumulator to instantiate — no
arbitrary function dispatch is needed.

---

## Sequences Catalog (Phase 20.2)

Standalone SQL sequences are stored in `axiom_sequences`, whose root page is
recorded in page 0 at `catalog_sequences_root`.

### `SequenceDef` — on-disk format

```
[schema_len: u8][schema: bytes]
[name_len:   u8][name:   bytes]
[last_value:  i64]
[start_value: i64]
[increment:   i64]
[min_value:   i64]
[max_value:   i64]
[cache_size:  u64]
[flags:       u8]   -- bit0 = CYCLE, bit1 = is_called
```

`NEXTVAL` uses a short internal transaction that commits the updated
`SequenceDef` immediately. That deliberately differs from ordinary DML rollback:
once a sequence value is returned to a session, rollback does not make it
available again. `CURRVAL` is not stored in the catalog; it is tracked per
`SessionContext` as lowercase `schema.sequence -> i64`.

### Catalog APIs

| Function | Description |
|---|---|
| `CatalogWriter::create_sequence(def)` | Persists a new `SequenceDef` |
| `CatalogWriter::replace_sequence_state(def)` | Replaces the visible sequence state after `NEXTVAL` |
| `CatalogReader::get_sequence(schema, name)` | Looks up a visible sequence by schema + name |
| `CatalogWriter::delete_sequence(schema, name)` | Removes a sequence definition |

---

## Enum Type Catalog (Phase 20.3)

Schema-scoped enum types are stored in `axiom_enum_types`, whose root page is
recorded in page 0 at `catalog_enum_types_root`.

### `EnumTypeDef` — on-disk format

```
[schema_len: u8][schema: bytes]
[name_len:   u8][name:   bytes]
[label_count: u16]
repeated label_count times:
  [label_len: u16][label: bytes]
```

Enum columns keep `ColumnType::Text` as the physical row type and persist the
declared enum identity in `ColumnDef.enum_type_name` as `schema.type`. The SQL
executor validates INSERT/UPDATE-family writes by loading the referenced
`EnumTypeDef` and checking the incoming text label against the stored label
list. Metadata paths use `enum_type_name` so `SHOW COLUMNS`, `SHOW CREATE
TABLE`, and `information_schema.COLUMNS` report the declared enum type.

### Catalog APIs

| Function | Description |
|---|---|
| `CatalogWriter::create_enum_type(def)` | Persists a new `EnumTypeDef` |
| `CatalogReader::get_enum_type(schema, name)` | Looks up a visible enum type |
| `CatalogReader::list_enum_types_in_schema(schema)` | Lists enum types in a schema |
| `CatalogWriter::delete_enum_type(schema, name)` | Removes an enum type definition |

---

## Array Column Catalog (Phase 20.4)

Array columns use `ColumnType::Array` in `ColumnDef.col_type` and store the
element type in an optional trailer appended to the `axiom_columns` row.

### `ColumnDef` binary format for array columns

`axiom_columns` rows use a backward-compatible trailing-field encoding:

```text
[base fields: col_idx, name, col_type, flags, ...]
[optional collation: len:2 LE + utf8 bytes]     -- when bit3 of flags set
[optional enum_type_name: len:2 LE + utf8]      -- when bit4 of flags set
[optional array_element_type: 1 byte tag]        -- when bit5 of flags set
```

**Serialization invariant:** when `enum_type_name` is present but `collation`
is absent, the encoder still emits a zero-length collation field. This ensures
the decoder never misidentifies enum bytes as collation bytes.

Element type tags mirror `ColumnType` discriminants (e.g., `1 = Int`,
`2 = BigInt`, `8 = Text`). Array columns with element type `Text` are the
common case (`TEXT[]`, `VARCHAR[]`); other types are explicitly tagged.

### `DROP TYPE`

`DROP TYPE [IF EXISTS] schema.name` removes a `EnumTypeDef` from
`axiom_enum_types`. If the type does not exist and `IF EXISTS` is omitted, the
executor returns `DbError::InvalidValue`. Dropping a type used by existing
columns does not cascade — column definitions continue to reference the type
name, but new validation against the (now-missing) type will fail.

---

## Regular Views Catalog (Phase 20.1)

### `RelationKind::View`

`TableDef.relation_kind` gains a third variant:

```rust
pub enum RelationKind {
    Table,            // tag 0 — physical heap or clustered table
    MaterializedView, // tag 1 — heap with defining_query
    View,             // tag 2 — NO physical pages; defining_query stores raw SQL
}
```

A `View` entry always has `root_page_id = 0`. The catalog never allocates B+Tree or heap pages for it.

### On-disk format (`axiom_tables`)

`TableDef` serialization extended with optional `relation_kind` and `defining_query` trailer (backward-compatible with older rows that omit the trailer):

```text
[base fields: table_id, root_page_id, schema_len, schema, name_len, name]
[optional flags byte:
  bit0-1 = StorageLayout (0=Heap, 1=Clustered)
  bit2 = defining_query present
  bit3 = relation_kind present (tag follows)
  bit4 = triggers present
  bit5 = collation_name present
]
[schema_version: 8 bytes LE]   -- present when flags byte is present
[defining_query: len:2 LE + utf8 bytes]  -- when bit2 set
[relation_kind: u8]            -- 0=Table, 1=MatView, 2=View; when bit3 set
...
```

Old rows that predate Phase 20.1 decode `relation_kind` as `Table` by default.

### Catalog APIs

| Function | Description |
|---|---|
| `CatalogWriter::create_view(schema, name, query)` | Creates a `RelationKind::View` entry with `root_page_id=0` |
| `CatalogWriter::replace_view_query(table_id, query)` | Updates the stored SQL for `CREATE OR REPLACE VIEW` |
| `TableDef::is_view()` | Returns `true` when `relation_kind == View` |

### View expansion in the analyzer

Views are expanded at analysis time, not execution time. In `analyzer_stmt.rs`:

1. `expand_views(stmt, storage, snapshot, ...)` — walks every `FromClause` in the statement.
2. When a `FromClause::Table(tref)` resolves to a `RelationKind::View`, the stored `defining_query` SQL is re-parsed.
3. The inner `SelectStmt` is recursively expanded (handles nested views).
4. The `FromClause` is replaced with `FromClause::Subquery { query: inner_select, alias, lateral: false }`.
5. Circular references are detected via an `expanding: HashSet<String>` that tracks which views are currently being expanded.

This means the executor never observes view references — they are fully rewritten before execution begins.
