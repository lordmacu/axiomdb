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
}

pub enum TableStorageLayout {
    Heap = 0,
    Clustered = 1,
}

// Legacy on-disk format for axiom_tables rows:
// [table_id:4 LE][root_page_id:8 LE][schema_len:1][schema UTF-8][name_len:1][name UTF-8]
//
// Current on-disk format:
// [table_id:4 LE][root_page_id:8 LE][schema_len:1][schema UTF-8][name_len:1][name UTF-8][layout:1]
//
// If the trailing layout byte is absent, the row decodes as Heap.

pub struct ColumnDef {
    pub table_id:      u64,
    pub col_index:     usize,       // zero-based position within the table
    pub col_name:      String,
    pub data_type:     DataType,    // from axiomdb-core::types::DataType
    pub not_null:      bool,
    pub default_value: Option<String>,  // DEFAULT expression as source text
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
clustered DML is deferred, so heap-only executor paths explicitly reject
`TableStorageLayout::Clustered` instead of touching the wrong page format.

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
