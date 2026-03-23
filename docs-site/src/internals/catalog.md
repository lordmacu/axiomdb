# Catalog System

The catalog is NexusDB's schema repository. It stores the definition of every table,
column, and index, and makes that information available to the SQL analyzer and
executor through a consistent, MVCC-aware reader interface.

---

## Design Goals

- **Self-describing:** The catalog tables are themselves stored as regular heap pages
  indexed by a B+ Tree. The engine needs no external schema file.
- **Persistent:** Catalog data survives crashes. The WAL treats catalog mutations like
  any other transaction.
- **MVCC-visible:** A DDL statement that creates a table is visible to subsequent
  statements in the same transaction but invisible to concurrent transactions until
  committed.
- **Bootstrappable:** An empty database file contains no catalog rows. The first
  `open()` runs a special bootstrap transaction that creates the three system tables.

---

## System Tables

The catalog consists of three tables stored in the `nexus` internal schema.
Their structure is described in [Catalog & Schema](../user-guide/features/catalog.md).

| Table             | Contents                                  |
|-------------------|-------------------------------------------|
| `nexus_tables`    | One row per user-visible table            |
| `nexus_columns`   | One row per column, in declaration order  |
| `nexus_indexes`   | One row per index                         |

These tables are themselves stored in heap pages and indexed by a B+ Tree keyed on
`(table_id, col_index)` for `nexus_columns` and `table_id` for `nexus_indexes`.

---

## CatalogBootstrap

`CatalogBootstrap` is a one-time procedure that runs when `open()` encounters an empty
database file (or a file with the meta page uninitialized).

### Bootstrap Sequence

```
1. Allocate page 0 (Meta page).
   Write format_version, zero for catalog_root_page, freelist_root_page, etc.

2. Allocate the freelist root page.
   Initialize the bitmap (all pages allocated so far are marked used).
   Write freelist_root_page into the meta page.

3. Allocate B+ Tree pages for nexus_tables.
   Insert the row describing nexus_tables into nexus_tables itself.
   Write catalog_root_page into the meta page.

4. Allocate B+ Tree pages for nexus_columns.
   Insert the column definitions for nexus_tables, nexus_columns, nexus_indexes.

5. Allocate B+ Tree pages for nexus_indexes.
   Insert the index definitions for the three system tables' own indexes.

6. Write a COMMIT WAL entry with LSN 1.
   Flush pages and WAL.
   Mark the meta page as bootstrapped.
```

The bootstrap uses txn_id = 0 and LSN 1. These special values are never reused. If
a crash occurs during bootstrap, the meta page checksum is invalid (step 6 has not
completed), so the next `open()` detects an uninitialized meta page and re-runs
the bootstrap from scratch. The re-bootstrap is safe: no data has been committed.

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
    pub fn list_tables(&self, schema: &str) -> Result<Vec<TableDef>, DbError>;

    /// Find a specific table by schema + name.
    pub fn find_table(&self, schema: &str, name: &str) -> Result<Option<TableDef>, DbError>;

    /// List columns for a table in declaration order.
    pub fn list_columns(&self, table_id: u64) -> Result<Vec<ColumnDef>, DbError>;

    /// List indexes for a table.
    pub fn list_indexes(&self, table_id: u64) -> Result<Vec<IndexDef>, DbError>;
}
```

The `snapshot` parameter ensures catalog reads are MVCC-consistent. A DDL statement
that has not yet committed is invisible to other transactions' `CatalogReader`.

---

## Schema Types

```rust
pub struct TableDef {
    pub id:           u64,
    pub schema_name:  String,
    pub table_name:   String,
    pub column_count: usize,
    pub created_lsn:  u64,
}

pub struct ColumnDef {
    pub table_id:      u64,
    pub col_index:     usize,       // zero-based position within the table
    pub col_name:      String,
    pub data_type:     DataType,    // from nexusdb-core::types::DataType
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
    pub root_page_id: u64,          // B+ Tree root for this index
}
```

---

## DDL Mutations Through the Catalog

When the executor processes `CREATE TABLE`, it:

1. Opens a write transaction (or participates in the current one).
2. Inserts a row into `nexus_tables` with the new table's metadata.
3. Inserts one row per column into `nexus_columns`.
4. Creates B+ Tree pages for the new table's primary key (or first UNIQUE column).
5. Inserts the index definition into `nexus_indexes`.
6. Appends all these mutations to the WAL.
7. Commits (or defers the commit to the surrounding transaction).

Because the catalog is stored in heap pages and indexed like any other table, all
crash recovery mechanisms apply automatically: WAL replay will reconstruct the catalog
state after a crash in the middle of `CREATE TABLE`, just as it would reconstruct
any other table mutation.

---

## Catalog Page Organization

```
Page 0:      Meta page (format_version, catalog_root_page, freelist_root_page, ...)
Page 1:      FreeList bitmap root
Pages 2–N:   B+ Tree pages for nexus_tables
Pages N+1–M: Heap pages for nexus_tables row data
Pages M+1–P: B+ Tree pages for nexus_columns
...
Pages P+1–Q: User table data begins here
```

The exact page assignments depend on database growth. Page 0 always remains the meta
page. All other page assignments are dynamic — the freelist tracks which pages are
in use, and the meta page records the root page IDs for each catalog B+ Tree.

---

## Catalog Invariants

The following invariants must hold at all times. The post-recovery integrity checker
(`nexusdb-embedded::integrity`) verifies them after crash recovery:

1. Every table listed in `nexus_tables` has at least one row in `nexus_columns`.
2. Every column in `nexus_columns` references a `table_id` that exists in `nexus_tables`.
3. Every index in `nexus_indexes` references a `table_id` that exists in `nexus_tables`.
4. Every `root_page_id` in `nexus_indexes` points to a page of type `Index`.
5. Every column listed in an index definition exists in the referenced table.
6. No two tables in the same schema have the same name.
7. No two indexes on the same table have the same name.

If any invariant is violated after recovery, NexusDB enters a read-only safe mode and
requires manual intervention.
