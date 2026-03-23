# Spec: 3.11 — Catalog Bootstrap

## What to build (not how)

The physical infrastructure for the database catalog: fixed-location storage of
root page IDs for the three system tables (`axiom_tables`, `axiom_columns`,
`axiom_indexes`), plus the in-memory schema types that both bootstrap (3.11)
and the reader/writer (3.12) share.

## Meta page extension — catalog header

The Meta page (page 0) body is extended with catalog page IDs:

```text
body[0..8]   db_magic: u64            (existing — DbFileMeta)
body[8..12]  db_version: u32          (existing)
body[12..16] _pad: u32                (existing)
body[16..24] page_count: u64          (existing)
body[24..32] checkpoint_lsn: u64      (3.6)
body[32..40] catalog_tables_root: u64  ← NEW: root page of axiom_tables heap (0 = uninit)
body[40..48] catalog_columns_root: u64 ← NEW: root page of axiom_columns heap (0 = uninit)
body[48..56] catalog_indexes_root: u64 ← NEW: root page of axiom_indexes heap (0 = uninit)
body[56..60] catalog_schema_ver: u32   ← NEW: 0 = uninitialized, 1 = v1 initialized
body[60..64] _catalog_pad: u32         ← reserved
```

`catalog_schema_ver` is the presence check: if `> 0`, the catalog is initialized
and the root page IDs are valid.

## System table schema

### axiom_tables (one row per user table)

```
table_id: u32     — unique, auto-incremented from 1 (0 = invalid)
schema_name: &str — e.g. "public"
table_name: &str  — e.g. "users"
```

### axiom_columns (one row per column)

```
table_id: u32
col_idx: u16      — 0-based column position
col_type: u8      — ColumnType discriminant
flags: u8         — bit0 = nullable
col_name: &str
```

### axiom_indexes (one row per index)

```
index_id: u32     — unique, auto-incremented from 1
table_id: u32
root_page_id: u64 — B+ Tree root page
flags: u8         — bit0 = unique, bit1 = primary key
index_name: &str
```

## Schema types (axiomdb-catalog/src/schema.rs)

```rust
pub type TableId = u32;

#[repr(u8)]
pub enum ColumnType {
    Bool      = 1,
    Int       = 2,  // i32
    BigInt    = 3,  // i64
    Float     = 4,  // f64
    Text      = 5,
    Bytes     = 6,
    Timestamp = 7,  // i64 µs since epoch
    Uuid      = 8,  // [u8; 16]
}

pub struct TableDef {
    pub id: TableId,
    pub schema_name: String,
    pub table_name: String,
}

pub struct ColumnDef {
    pub table_id: TableId,
    pub col_idx: u16,
    pub name: String,
    pub col_type: ColumnType,
    pub nullable: bool,
}

pub struct IndexDef {
    pub table_id: TableId,
    pub name: String,
    pub root_page_id: u64,
    pub is_unique: bool,
    pub is_primary: bool,
}
```

### Row serialization (binary, compact)

Each schema type serializes to bytes for storage in heap slots:

**TableRow** (variable length):
```
[table_id: u32 LE][schema_len: u8][schema bytes][name_len: u8][name bytes]
```

**ColumnRow** (variable length):
```
[table_id: u32 LE][col_idx: u16 LE][col_type: u8][flags: u8][name_len: u8][name bytes]
```

**IndexRow** (variable length):
```
[table_id: u32 LE][root_page_id: u64 LE][flags: u8][name_len: u8][name bytes]
```

Serialization errors: `DbError::ParseError` for invalid bytes.

## CatalogBootstrap (axiomdb-catalog/src/bootstrap.rs)

```rust
pub struct CatalogPageIds {
    pub tables: u64,   // root heap page of axiom_tables
    pub columns: u64,  // root heap page of axiom_columns
    pub indexes: u64,  // root heap page of axiom_indexes
}

pub struct CatalogBootstrap;

impl CatalogBootstrap {
    /// Returns true if the catalog has been initialized on this database.
    pub fn is_initialized(storage: &dyn StorageEngine) -> Result<bool, DbError>

    /// Initializes the catalog on a freshly created database:
    /// allocates one heap root page per system table, writes their IDs
    /// and catalog_schema_ver=1 to the meta page, flushes to disk.
    ///
    /// No-op if already initialized (idempotent).
    pub fn init(storage: &mut dyn StorageEngine) -> Result<CatalogPageIds, DbError>

    /// Reads the catalog page IDs from the meta page.
    /// Returns Err if catalog is not initialized.
    pub fn page_ids(storage: &dyn StorageEngine) -> Result<CatalogPageIds, DbError>
}
```

## Use cases

1. **Fresh database creation**: `CatalogBootstrap::init(storage)` allocates pages,
   writes IDs to meta page. Subsequent `is_initialized` returns true. ✓

2. **Reopen existing database**: `is_initialized` returns true.
   `page_ids` returns the stored IDs. ✓

3. **Double-init (idempotent)**: calling `init` twice returns the SAME page IDs
   (reads existing IDs if already initialized). ✓

4. **Uninitialized database**: `page_ids` returns Err(CatalogNotInitialized). ✓

5. **TableRow round-trip**: `TableDef::to_bytes()` → `TableDef::from_bytes()` = same struct. ✓

6. **ColumnRow round-trip**: same for ColumnDef. ✓

7. **IndexRow round-trip**: same for IndexDef. ✓

8. **ColumnType from u8**: known discriminants parse correctly; unknown byte → Err. ✓

## Acceptance criteria

- [ ] `catalog_tables_root` / `catalog_columns_root` / `catalog_indexes_root` constants
      defined at body[32], [40], [48] and NOT overlapping with `page_count` or `checkpoint_lsn`
- [ ] `catalog_schema_ver` constant at body[56]
- [ ] `is_initialized` returns false on fresh database
- [ ] `init` sets `catalog_schema_ver = 1` in meta page
- [ ] `init` is idempotent (calling twice returns same IDs, no new pages)
- [ ] `page_ids` returns error when not initialized
- [ ] `page_ids` returns correct IDs after init
- [ ] After `init` + flush + reopen (MmapStorage), `page_ids` returns same IDs
- [ ] `TableDef::to_bytes()` / `from_bytes()` roundtrip
- [ ] `ColumnDef::to_bytes()` / `from_bytes()` roundtrip
- [ ] `IndexDef::to_bytes()` / `from_bytes()` roundtrip
- [ ] `ColumnType::try_from(u8)` for all valid values
- [ ] `ColumnType::try_from(0)` returns Err (invalid discriminant)
- [ ] No `unwrap()` in src/

## New DbError variant

```rust
#[error("catalog not initialized — call CatalogBootstrap::init() first")]
CatalogNotInitialized,
```

## Out of scope

- Reading/writing actual catalog rows (3.12)
- Auto-increment ID management (3.12)
- Index-backed catalog lookups (3.12)
- Schema validation (3.14)

## Dependencies

- `axiomdb-storage`: `StorageEngine`, `Page`, `PageType`, `HEADER_SIZE` → must be added to Cargo.toml
- `axiomdb-core`: `DbError`, `TxnId` (already in deps)
- New `DbError::CatalogNotInitialized` variant in axiomdb-core
