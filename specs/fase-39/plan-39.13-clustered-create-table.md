# Plan: 39.13 clustered CREATE TABLE

## Files to create/modify

- `crates/axiomdb-catalog/src/schema.rs`
  - extend `TableDef` with storage-layout metadata
  - rename the table root field to a generic root concept
  - keep binary compatibility for legacy heap table rows
- `crates/axiomdb-catalog/src/writer.rs`
  - add clustered-table creation support
  - generalize the existing root-update helper away from heap-specific naming
- `crates/axiomdb-catalog/src/reader.rs`
  - no new behavior expected, but adapt to the new `TableDef` layout/field names
- `crates/axiomdb-sql/src/executor/ddl.rs`
  - detect explicit `PRIMARY KEY`
  - choose heap vs clustered table creation path
  - persist logical primary-index metadata for clustered tables
- `crates/axiomdb-sql/src/table.rs`
  - add explicit guard rails for heap-only scan/insert/update/delete helpers
    when called on clustered tables
- `crates/axiomdb-sql/src/executor/select.rs`
  - fail early with a clustered-not-yet-implemented error instead of touching
    heap paths on clustered tables
- `crates/axiomdb-sql/src/executor/insert.rs`
  - fail early on clustered tables until `39.14`
- `crates/axiomdb-sql/src/executor/update.rs`
  - fail early on clustered tables until `39.16`
- `crates/axiomdb-sql/src/executor/delete.rs`
  - fail early on clustered tables until `39.17`
- `crates/axiomdb-sql/tests/integration_clustered_create_table.rs`
  - add end-to-end clustered CREATE TABLE coverage
- `docs/fase-39.md`
  - close `39.13` with explicit dual-mode DDL semantics
- `docs/progreso.md`
  - mark `39.13` complete and note deferred gaps
- `docs-site/src/internals/storage.md`
  - document heap-vs-clustered table root metadata
- `docs-site/src/internals/catalog.md`
  - document the new `TableDef` layout and legacy decoding rule
- `docs-site/src/internals/architecture.md`
  - document executor guard rails before `39.14+`
- `docs-site/src/user-guide/features/indexes.md`
  - document that tables with explicit PK now bootstrap clustered storage
- `docs-site/src/user-guide/features/transactions.md`
  - note that clustered DML is still deferred even though clustered DDL exists
- `docs-site/src/development/roadmap.md`
  - advance roadmap to `39.13`
- `memory/project_state.md`
  - record the new clustered-table DDL boundary
- `memory/architecture.md`
  - record the catalog/runtime split between heap and clustered tables
- `memory/lessons.md`
  - record why hidden clustered keys were rejected here

## Algorithm / Data structure

### 1. Extend `TableDef` for dual-mode storage

Add a storage-layout enum:

```rust
enum TableStorageLayout {
    Heap,
    Clustered,
}
```

And store:

```rust
TableDef {
  id,
  root_page_id,
  storage_layout,
  schema_name,
  table_name,
}
```

Binary compatibility rule:

```text
old row:
  [table_id][root_page_id][schema_len][schema][name_len][name]

new row:
  [table_id][root_page_id][schema_len][schema][name_len][name][flags]

if trailing flags byte is absent:
  storage_layout = Heap
```

This keeps pre-39.13 catalog rows readable without migration.

### 2. Split table creation by layout

Introduce a layout-aware catalog helper:

```text
create_table_with_layout(schema, name, layout):
  allocate table_id
  if layout == Heap:
    alloc PageType::Data root
    init empty heap page
  else if layout == Clustered:
    alloc PageType::ClusteredLeaf root
    init empty clustered leaf page
  persist TableDef with selected layout + root_page_id
```

Keep the old `create_table(...)` entry point as a heap-default wrapper if that
reduces blast radius.

### 3. Detect explicit PK in `CREATE TABLE`

In `execute_create_table(...)`:

```text
collect primary key columns from:
  - inline ColumnConstraint::PrimaryKey
  - table-level TableConstraint::PrimaryKey

if explicit PK exists:
  layout = Clustered
else:
  layout = Heap

table_id = create_table_with_layout(schema, name, layout)
```

For clustered tables:

```text
create logical primary IndexDef:
  table_id = table_id
  root_page_id = table.root_page_id
  is_primary = true
  columns = PK columns in declaration order
```

Secondary/unique indexes remain normal ordered containers over `PageType::Index`
roots, as in the current executor.

### 4. Guard rails before `39.14+`

Every heap-only path that accepts `&TableDef` must branch first:

```text
if table_def.storage_layout == Clustered:
  return DbError::NotImplemented {
    feature: "clustered <operation> — Phase 39.14/39.15/39.16/39.17"
  }
```

This includes:

- table scan helpers
- row insert/update/delete helpers
- executor select/insert/update/delete entry points that would otherwise walk
  the heap path

The goal is explicit failure, not best-effort fallback.

## Implementation phases

1. Extend `TableDef` with storage-layout metadata and generic root naming while
   preserving legacy catalog decode.
2. Add layout-aware table creation in `CatalogWriter`, including empty
   clustered-leaf initialization.
3. Update DDL `CREATE TABLE` to select clustered layout for explicit-PK tables.
4. Persist logical primary-index metadata for clustered tables.
5. Add guard rails in heap-only runtime paths so clustered tables error
   explicitly before `39.14+`.
6. Add targeted unit/integration coverage for clustered create-table semantics
   and guard-rail behavior.
7. Close docs/progress/memory after targeted validation passes.

## Tests to write

- unit:
  - `TableDef` new-format round-trip preserves `storage_layout`
  - legacy `TableDef` bytes decode as `Heap`
  - clustered `CatalogWriter` creation allocates `PageType::ClusteredLeaf`
  - heap `CatalogWriter` creation still allocates `PageType::Data`
- integration:
  - `CREATE TABLE ... PRIMARY KEY ...` produces a clustered table definition
    with a clustered root page
  - `CREATE TABLE` without PK still produces a heap table definition
  - clustered table creation persists a primary-index catalog row with the
    correct PK column order
  - `INSERT` on a clustered table fails with the expected `NotImplemented`
    error before `39.14`
  - `SELECT` on a clustered table fails with the expected `NotImplemented`
    error before `39.15`
- bench:
  - none in `39.13`; this is catalog/DDL plumbing and guard rails

## Anti-patterns to avoid

- Do not invent a hidden clustered key for PK-less tables in this subphase.
- Do not keep overloading the catalog as if every root were a heap chain root.
- Do not let heap helpers touch clustered roots “just because the table is
  empty”.
- Do not create a fake heap primary index alongside the clustered root.
- Do not mix clustered DML implementation into this subphase.

## Risks

- Legacy catalog decode regression:
  mitigate by making the new `TableDef` flags byte optional on read.
- Existing heap callers break on field rename:
  mitigate by updating the full `TableDef` call surface in one cut instead of
  leaving mixed naming.
- Clustered tables accidentally enter old heap DML paths:
  mitigate with explicit guard rails in both executor entry points and
  `TableEngine`.
- Primary-index metadata diverges from table root:
  mitigate by using the same clustered root page id as the logical PK index root
  from day one.
