# Plan: 3.11 — Catalog Bootstrap

## Files to create / modify

| File | Action | What |
|---|---|---|
| `crates/axiomdb-core/src/error.rs` | modify | Add `CatalogNotInitialized` |
| `crates/axiomdb-storage/src/meta.rs` | modify | Add catalog header constants + read/write |
| `crates/axiomdb-storage/src/lib.rs` | modify | Export new catalog meta symbols |
| `crates/axiomdb-catalog/Cargo.toml` | modify | Add `axiomdb-storage` dependency |
| `crates/axiomdb-catalog/src/schema.rs` | create | `ColumnType`, `TableDef`, `ColumnDef`, `IndexDef` + serialization |
| `crates/axiomdb-catalog/src/bootstrap.rs` | create | `CatalogPageIds`, `CatalogBootstrap` |
| `crates/axiomdb-catalog/src/lib.rs` | modify | Export all public types |

## Step 1 — DbError::CatalogNotInitialized

```rust
#[error("catalog not initialized — call CatalogBootstrap::init() first")]
CatalogNotInitialized,
```

## Step 2 — meta.rs: catalog header constants

Add to `axiomdb-storage/src/meta.rs` after `CHECKPOINT_LSN_BODY_OFFSET`:

```rust
/// body[32..40]: root heap page of nexus_tables (0 = uninitialized)
pub const CATALOG_TABLES_ROOT_BODY_OFFSET: usize = 32;
/// body[40..48]: root heap page of nexus_columns (0 = uninitialized)
pub const CATALOG_COLUMNS_ROOT_BODY_OFFSET: usize = 40;
/// body[48..56]: root heap page of nexus_indexes (0 = uninitialized)
pub const CATALOG_INDEXES_ROOT_BODY_OFFSET: usize = 48;
/// body[56..60]: catalog schema version (0 = uninitialized, 1 = v1)
pub const CATALOG_SCHEMA_VER_BODY_OFFSET: usize = 56;

const _: () = assert!(CATALOG_SCHEMA_VER_BODY_OFFSET + 4 <= PAGE_SIZE - HEADER_SIZE,
    "catalog header must fit in meta page body");

// ── Catalog meta page read/write ─────────────────────────────────────────────

pub fn read_catalog_schema_ver(storage: &dyn StorageEngine) -> Result<u32, DbError> {
    let page = storage.read_page(0)?;
    let off = HEADER_SIZE + CATALOG_SCHEMA_VER_BODY_OFFSET;
    Ok(u32::from_le_bytes([raw[off], raw[off+1], raw[off+2], raw[off+3]]))
}

pub fn read_catalog_page_id(storage: &dyn StorageEngine, body_offset: usize) -> Result<u64, DbError> {
    let page = storage.read_page(0)?;
    let off = HEADER_SIZE + body_offset;
    Ok(u64::from_le_bytes([raw[off]..raw[off+8]]))
}

/// Writes all catalog header fields atomically (one write_page call).
pub fn write_catalog_header(
    storage: &mut dyn StorageEngine,
    tables_root: u64,
    columns_root: u64,
    indexes_root: u64,
    schema_ver: u32,
) -> Result<(), DbError> {
    let bytes = *storage.read_page(0)?.as_bytes();
    let mut page = Page::from_bytes(bytes)?;
    let raw = page.as_bytes_mut();

    let base = HEADER_SIZE;
    raw[base+32..base+40].copy_from_slice(&tables_root.to_le_bytes());
    raw[base+40..base+48].copy_from_slice(&columns_root.to_le_bytes());
    raw[base+48..base+56].copy_from_slice(&indexes_root.to_le_bytes());
    raw[base+56..base+60].copy_from_slice(&schema_ver.to_le_bytes());

    page.update_checksum();
    storage.write_page(0, &page)?;
    Ok(())
}
```

Export from `lib.rs`:
```rust
pub use meta::{
    read_catalog_page_id, read_catalog_schema_ver, write_catalog_header,
    CATALOG_COLUMNS_ROOT_BODY_OFFSET, CATALOG_INDEXES_ROOT_BODY_OFFSET,
    CATALOG_SCHEMA_VER_BODY_OFFSET, CATALOG_TABLES_ROOT_BODY_OFFSET,
    CHECKPOINT_LSN_BODY_OFFSET, read_checkpoint_lsn, write_checkpoint_lsn,
};
```

## Step 3 — axiomdb-catalog/Cargo.toml

```toml
[dependencies]
axiomdb-core    = { workspace = true }
axiomdb-storage = { workspace = true }
thiserror       = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
```

## Step 4 — schema.rs: ColumnType

```rust
/// SQL column type stored in the catalog. A subset of DataType from axiomdb-core
/// sufficient for Phase 3-4; extended in later phases.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    Bool      = 1,
    Int       = 2,  // i32
    BigInt    = 3,  // i64
    Float     = 4,  // f64
    Text      = 5,
    Bytes     = 6,
    Timestamp = 7,  // i64 microseconds since UTC epoch
    Uuid      = 8,  // [u8; 16]
}

impl TryFrom<u8> for ColumnType {
    type Error = DbError;
    fn try_from(v: u8) -> Result<Self, Self::Error> { ... }
}

impl From<ColumnType> for u8 { fn from(c: ColumnType) -> u8 { c as u8 } }
```

## Step 5 — schema.rs: TableDef + serialization

```rust
pub struct TableDef {
    pub id: TableId,
    pub schema_name: String,
    pub table_name: String,
}

impl TableDef {
    /// Serializes to: [table_id:4][schema_len:1][schema bytes][name_len:1][name bytes]
    pub fn to_bytes(&self) -> Vec<u8>

    /// Deserializes from bytes. Returns (TableDef, bytes_consumed).
    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, usize), DbError>
}
```

## Step 6 — schema.rs: ColumnDef + IndexDef + serialization

Same pattern:

**ColumnRow**: `[table_id:4][col_idx:2][col_type:1][flags:1][name_len:1][name bytes]`
- `flags`: bit0 = nullable

**IndexRow**: `[table_id:4][root_page_id:8][flags:1][name_len:1][name bytes]`
- `flags`: bit0 = unique, bit1 = primary

## Step 7 — bootstrap.rs

```rust
pub struct CatalogPageIds {
    pub tables: u64,
    pub columns: u64,
    pub indexes: u64,
}

pub struct CatalogBootstrap;

impl CatalogBootstrap {
    pub fn is_initialized(storage: &dyn StorageEngine) -> Result<bool, DbError> {
        let ver = read_catalog_schema_ver(storage)?;
        Ok(ver > 0)
    }

    pub fn init(storage: &mut dyn StorageEngine) -> Result<CatalogPageIds, DbError> {
        // Idempotent: if already initialized, return existing IDs.
        if Self::is_initialized(storage)? {
            return Self::page_ids(storage);
        }

        // Allocate one heap root page per system table.
        let tables_root  = storage.alloc_page(PageType::Data)?;
        let columns_root = storage.alloc_page(PageType::Data)?;
        let indexes_root = storage.alloc_page(PageType::Data)?;

        // Initialize each page as an empty heap page (valid checksum).
        for &page_id in &[tables_root, columns_root, indexes_root] {
            let mut page = Page::new(PageType::Data, page_id);
            page.update_checksum(); // already done by Page::new, but explicit is clear
            storage.write_page(page_id, &page)?;
        }

        // Write catalog header to meta page.
        write_catalog_header(storage, tables_root, columns_root, indexes_root, 1)?;
        storage.flush()?;

        Ok(CatalogPageIds { tables: tables_root, columns: columns_root, indexes: indexes_root })
    }

    pub fn page_ids(storage: &dyn StorageEngine) -> Result<CatalogPageIds, DbError> {
        if !Self::is_initialized(storage)? {
            return Err(DbError::CatalogNotInitialized);
        }
        let tables  = read_catalog_page_id(storage, CATALOG_TABLES_ROOT_BODY_OFFSET)?;
        let columns = read_catalog_page_id(storage, CATALOG_COLUMNS_ROOT_BODY_OFFSET)?;
        let indexes = read_catalog_page_id(storage, CATALOG_INDEXES_ROOT_BODY_OFFSET)?;
        Ok(CatalogPageIds { tables, columns, indexes })
    }
}
```

## Step 8 — Tests

**schema.rs unit tests:**
```rust
fn test_column_type_roundtrip_all_variants()  // u8 → ColumnType → u8 for each
fn test_column_type_invalid_discriminant()    // try_from(0) and try_from(255) → Err
fn test_table_def_to_from_bytes()             // roundtrip
fn test_column_def_to_from_bytes()            // roundtrip with nullable flag
fn test_index_def_to_from_bytes()             // roundtrip with unique + primary flags
fn test_table_def_empty_strings()             // edge case: empty schema/name
fn test_from_bytes_truncated_input()          // error on too-short bytes
```

**bootstrap.rs tests (MemoryStorage + MmapStorage):**
```rust
fn test_fresh_db_not_initialized()
fn test_init_sets_schema_ver_1()
fn test_init_allocates_three_pages()
fn test_init_is_idempotent()              // call twice → same page IDs
fn test_page_ids_error_when_not_initialized()
fn test_page_ids_correct_after_init()
fn test_catalog_survives_reopen()         // MmapStorage: init → close → open → page_ids same
```

## Anti-patterns to avoid

- **NO** overlapping meta page body offsets — compile-time assertions verify
- **NO** changing FreeList reserved pages — catalog pages are allocated normally via alloc_page
- **NO** storing schema data in 3.11 — that's 3.12
- **NO** `unwrap()` in src/

## Risks

| Risk | Mitigation |
|---|---|
| body offsets overlap with DbFileMeta existing fields | Compile-time assert: 32 > 24+8=32 — wait, body[24..32] is checkpoint_lsn, body[32] starts catalog. No overlap ✓ |
| `write_catalog_header` doesn't update checksum | Explicit `page.update_checksum()` call before write_page |
| `init` on already-init DB allocates extra pages | `is_initialized` check at start → return early ✓ |

## Implementation order

```
1. error.rs: CatalogNotInitialized
2. meta.rs: catalog header constants + read/write functions
3. lib.rs: export new symbols
4. catalog/Cargo.toml: add axiomdb-storage
5. schema.rs: ColumnType + TryFrom<u8>
6. schema.rs: TableDef + to_bytes/from_bytes
7. schema.rs: ColumnDef + IndexDef + to_bytes/from_bytes
8. schema.rs: unit tests (7 tests)
9. bootstrap.rs: CatalogPageIds + CatalogBootstrap
10. bootstrap.rs: tests (7 tests)
11. lib.rs: export all
12. cargo test --workspace + clippy + fmt
```
