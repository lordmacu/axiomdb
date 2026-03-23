# Catalog System

The catalog is AxiomDB's schema repository. It stores the definition of every table,
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

The catalog consists of three tables stored in the `axiom` internal schema.
Their structure is described in [Catalog & Schema](../user-guide/features/catalog.md).

| Table             | Contents                                  |
|-------------------|-------------------------------------------|
| `axiom_tables`    | One row per user-visible table            |
| `axiom_columns`   | One row per column, in declaration order  |
| `axiom_indexes`   | One row per index                         |

These tables are themselves stored in heap pages and indexed by a B+ Tree keyed on
`(table_id, col_index)` for `axiom_columns` and `table_id` for `axiom_indexes`.

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

3. Allocate B+ Tree pages for axiom_tables.
   Insert the row describing axiom_tables into axiom_tables itself.
   Write catalog_root_page into the meta page.

4. Allocate B+ Tree pages for axiom_columns.
   Insert the column definitions for axiom_tables, axiom_columns, axiom_indexes.

5. Allocate B+ Tree pages for axiom_indexes.
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
    pub id:                 u32,
    pub data_root_page_id:  u64,    // heap chain root for user row data
    pub schema_name:        String,
    pub table_name:         String,
}

// On-disk binary format for axiom_tables rows:
// [table_id:4 LE][data_root_page_id:8 LE][schema_len:1][schema UTF-8][name_len:1][name UTF-8]

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
    pub root_page_id: u64,          // B+ Tree root for this index
}
```

---

## DDL Mutations Through the Catalog

When the executor processes `CREATE TABLE`, it:

1. Opens a write transaction (or participates in the current one).
2. Allocates a new `TableId` from the meta page sequence.
3. Allocates a `Data` page as the heap root for user row data (`data_root_page_id`).
4. Inserts a row into `axiom_tables` with `{id, data_root_page_id, schema_name, table_name}`.
5. Inserts one row per column into `axiom_columns`.
6. Creates B+ Tree pages for the new table's primary key (or first UNIQUE column).
7. Inserts the index definition into `axiom_indexes`.
8. Appends all these mutations to the WAL.
9. Commits (or defers the commit to the surrounding transaction).

The `data_root_page_id` stored in `axiom_tables` is used by `TableEngine` (Phase 4.5b)
to locate the heap chain for all DML on that table — no extra lookup required.

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

The following invariants must hold at all times. The post-recovery integrity checker
(`axiomdb-embedded::integrity`) verifies them after crash recovery:

1. Every table listed in `axiom_tables` has at least one row in `axiom_columns`.
2. Every column in `axiom_columns` references a `table_id` that exists in `axiom_tables`.
3. Every index in `axiom_indexes` references a `table_id` that exists in `axiom_tables`.
4. Every `root_page_id` in `axiom_indexes` points to a page of type `Index`.
5. Every column listed in an index definition exists in the referenced table.
6. No two tables in the same schema have the same name.
7. No two indexes on the same table have the same name.

If any invariant is violated after recovery, AxiomDB enters a read-only safe mode and
requires manual intervention.
