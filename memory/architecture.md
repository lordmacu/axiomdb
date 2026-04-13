# Architecture Notes

## 2026-04-13 — `JSON_TABLE` flat row source (11.20a)

- **New module**: `crates/axiomdb-sql/src/json_table.rs`. Owns compile + materialize.
  - `JsonTableSpec` holds `Vec<PathStepOwned>` compiled once per statement
    (row path + every column PATH); `materialize_json_table` walks without
    re-parsing.
  - `PathStepOwned` is an owned, trivially-cloneable subset (`Root`, `Key`,
    `Index`, `WildcardKey`, `WildcardIndex`, `Recursive`) — distinct from the
    richer `PathStep` enum in `eval::functions::json` that carries `Expr`
    filter nodes (not needed here).
- **AST**: `FromClause::JsonTable(Box<JsonTable>)` with three column shapes:
  `Regular { ty, path, on_empty, on_error }`, `Ordinality { name }`,
  `Exists { ty, path, on_error }`. Reuses the Phase 11.19a
  `SqlJsonOnBehavior` enum (`Null | Error | Default(Expr) | TrueLit | FalseLit | Unknown`).
- **Parser dispatch**: `parse_from_item` uses `peek_at(1) == Token::LParen`
  to commit to the `JSON_TABLE` branch only when the identifier is followed
  by `(`. This keeps a user table literally named `json_table` working.
  `COLUMNS`, `PATH`, `ORDINALITY`, `EMPTY`, `UNKNOWN` are plain identifiers;
  `FOR`, `DEFAULT`, `EXISTS`, `NULL`, `TRUE`, `FALSE`, `ON`, `AS` are reserved
  tokens (`Token::For`, `Token::Default`, `Token::Exists`, etc.).
- **Analyzer**: `bound_from_clause` publishes `JsonTable` as a virtual
  `BoundTable` (columns built from the declared DataTypes + nullability from
  `on_empty=Null`). `analyzer_stmt::resolve_json_table` re-runs
  `resolve_expr_full` over the `doc` expression and every `DEFAULT expr`
  inside ON EMPTY / ON ERROR so correlation (through `OuterColumn`) and
  outer-scope binding stay consistent with subquery-in-FROM handling.
- **Executor**: First-FROM case goes through `execute_select_json_table_source`
  in `select_core.rs` (mirrors `execute_select_derived`). JOIN right-side
  case goes through a new arm in `select_joins_ctx.rs` that compiles + walks
  in-place. **Deliberate restriction for 11.20a**:
  1. `doc_has_column_refs` guards the JOIN path — any column reference in
     `doc` triggers a `NotImplemented` pointing to 11.20d (LATERAL semantics
     require per-left-row re-materialization).
  2. `JSON_TABLE` as the first FROM combined with JOIN also raises
     `NotImplemented` (generalizing `select_joins_ctx.rs` to accept a non-
     TableRef first source is 11.20d scope).
- **Semantics chosen**: `Value::Null` doc → zero rows (PG/MariaDB parity, not
  an error); invalid TEXT JSON → `InvalidCoercion`; scalar PATH miss →
  `ON EMPTY` (default `Null`); scalar type mismatch / multi-match →
  `ON ERROR` (default `Null`); `EXISTS` defaults `FALSE ON ERROR`.
- **Research reference**: PG `parse_jsontable.c` + `nodeTableFuncscan.c`
  use a flattened plan tree with a `SiblingJoin` node; MariaDB `json_table.cc`
  uses recursive sibling-walk with `m_ordinality_counter` per path. AxiomDB
  adopts MariaDB's model — cleaner fit for the `Vec<Row>` executor. When
  NESTED PATH lands in 11.20c the same module adds recursive child-walk
  without a dedicated plan-tree IR.

## 2026-04-10 - JSONB binary layout (11.16)

- `crates/axiomdb-types/src/jsonb.rs` owns the binary JSONB format:
  - container header (u32): bit31=array, bits30..0=count
  - JEntry array (u32 per element): bit31=HAS_OFF, bits30..28=type, bits27..0=len/offset
  - stride=32: every 32nd JEntry stores an absolute offset for O(1) random access
  - key sort order: bytewise-length-first (shorter keys first, then lexicographic)
  - `JsonbEncoder::encode` uses iterative DFS with explicit `Vec<Frame>` stack; depth limit 256
  - `JsonbRef::get_key` uses binary search over sorted key section; no heap alloc for lookup
- `Value::Jsonb(Arc<Vec<u8>>)` is the in-memory type; `DataType::Jsonb` discriminant 10; `ColumnType::Jsonb = 10`
- `->` operator lowers to `BinaryOp::JsonSub`; `->>` still lowers to `JSON_EXTRACT`
- JSONPath: `crates/axiomdb-sql/src/eval/jsonpath.rs` owns compiler + executor with lax/strict modes
- All existing Phase 11.4 JSON functions upgraded to binary path for `Value::Jsonb` input

## 2026-04-10 - Refcounted TOAST/BLOB overflow chains (11.2d)

- `crates/axiomdb-storage/src/clustered_overflow.rs` has two compatible overflow-chain contracts:
  - legacy clustered row overflow: body starts with `next_page: u64`, then payload bytes
  - refcounted TOAST/BLOB overflow: body starts with `ABOB`, version, flags, `next_page`, `part_len`, first-page `refcount`
- `read_blob_chain()` is the compatibility boundary: detects `ABOB` or falls back to legacy path
- `free_blob()` decrements first-page refcount; frees chain only at zero
- Row codec TOAST placeholder: `__toast__:page_id:compressed:raw_len`

## 2026-04-10 - Native JSON boundary (11.4)

- `Value::Json(String)` uses u24 length-prefixed payload, same shape as TEXT
- `DataType::Json = 9` → decode arm returns `Value::Json`, not `Value::Text`
- `data->>'field'` lowers to `JSON_EXTRACT(data, '$.field')` (no new BinaryOp)
- Wire: JSON exposed as string-compatible payload on MySQL wire

## 2026-04-09 — ALTER TABLE + ANSI quote mode hardening

- `ddl_alter_constraint.rs`: `ALTER TABLE ... ADD PRIMARY KEY` → staged heap→clustered migration
- `ddl_alter_column.rs`: indexed DROP/MODIFY repair matrix (PK/FK/CHECK dependencies, heap vs clustered rewrite)
- `catalog/writer.rs`: `replace_index_def`, `replace_foreign_key` for MVCC-safe replacement
- `lexer.rs`: `Token::AtAt` (`@@`), `Token::At` (`@`); `ansi_quotes` bit shared across parser and wire helpers

## 2026-04-03 — Aggregate hash execution + zero-alloc clustered scan (39.21)

- `GroupTablePrimitive` (INT/BIGINT single-col GROUP BY) and `GroupTableGeneric` (multi-col/TEXT)
- `scan_all_callback` in `clustered_tree.rs`: callback-based scan, zero extra allocation for inline rows
- `scan_clustered_table_masked(mask)` backed by `scan_all_callback`

## 2026-04-03 — Clustered VACUUM and root-persistence fix (39.18)

- Clustered purge: descend once to leftmost leaf, walk `next_leaf`, remove cells where delete-mark is safe
- `BTree::delete_many_in` may rotate/collapse root → VACUUM must persist new root to catalog immediately

## 2026-04-02 — Clustered storage (Phase 39: 39.1–39.17)

- `clustered_internal.rs`: clustered internal page format
- `clustered_tree.rs`: clustered tree controller (insert/read/update/delete/rebalance + `scan_all_callback`)
- `clustered_overflow.rs`: overflow-page chain for large clustered rows
- `clustered_secondary.rs`: secondary key = `secondary_logical_key ++ missing_PK_columns`
- `clustered_tree.rs` + `clustered.rs` (WAL): row-image codec for WAL; key=PK bytes, payload=exact row image
- Catalog `TableDef`: `root_page_id` + `storage_layout` (heap vs clustered)
- CREATE TABLE with explicit PRIMARY KEY → clustered tree; without PK → heap
- Variable-size: split by byte volume, not key count; left page keeps old page ID

## 2026-03-29 — Integration test structure

- `axiomdb-sql/tests/common/mod.rs`: shared harness
- Tests split by execution path: `integration_executor`, `integration_executor_joins`, etc.
- One binary per cohesive execution path; split only on mixed responsibility

## 2026-03-27 — WAL fsync pipeline + transactional INSERT staging

- `axiomdb-wal/src/fsync_pipeline.rs`: leader-based fsync coalescing (Expired / Acquired / Queued)
- `PendingInsertBatch` in `SessionContext`: staging for explicit transactions only
- `executor/staging.rs`: `apply_insert_batch` shared by transactional staging and immediate multi-row INSERT

## 2026-03-26 — Executor + eval decomposition

- `executor/` directory: `mod.rs` facade + `shared`, `select`, `joins`, `aggregate`, `insert`, `update`, `delete`, `bulk_empty`, `ddl`
- `eval/` directory: `mod.rs` facade + `context`, `core`, `ops`, `functions/`
- `index_maintenance.rs`: `delete_many_from_indexes` for batch DELETE/UPDATE
- Stable-RID UPDATE: skip index rewrite only when RID stable AND logical key unchanged

## 2026-03-27 — Database catalog + DSN parsing

- `axiomdb-catalog`: `axiom_databases` + `axiom_table_databases` relations
- Pre-22b.3a tables readable without migration (default to `axiomdb` db)
- `axiomdb-core/src/dsn.rs`: `ParsedDsn::Wire` vs `ParsedDsn::Local`

## 2026-03-26 — Connection lifecycle

- `axiomdb-network/src/mysql/lifecycle.rs`: explicit `CONNECTED → AUTH → IDLE → EXECUTING → CLOSING`
- `ConnectionLifecycle` (timeout policy) separate from `ConnectionState` (SQL session state)
