# Architecture Notes

## 2026-04-22 - Advanced SQL acceptance suite (21.23)

- **`21.23` is a consolidation layer, not a feature layer.** The new
  `integration_advanced_sql.rs` file deliberately composes already-implemented
  Phase 21 features in multi-statement flows instead of duplicating parser or
  single-feature executor tests.
- **Session-heavy features need interaction tests, not only isolated ones.**
  Savepoints, cursors, `MERGE`, and `CHECKPOINT` can all pass in their own
  files while still drifting at the boundaries between transaction lifecycle,
  session state, and mixed DML/admin flows.
- **The wire harness runs with `autocommit=False`, so smoke blocks must settle
  their own setup transactions before explicit `BEGIN`.** A `CREATE TABLE` or
  setup `INSERT` in the same block can otherwise leave an implicit transaction
  open and make the next explicit `BEGIN` fail for harness reasons, not engine
  reasons.

## 2026-04-22 - Query hints (21.11)

- **`21.11` is comment-preservation first, not planner syntax first.**
  MySQL-style optimizer comments were impossible until `lexer.rs` stopped
  dropping `/*+ ... */` together with ordinary block comments; the parser work
  depends on that preservation boundary.
- **Bounded hint enums are cheaper than a generic hint framework.**
  `SelectHint::{Index, HashJoin, Parallel}` is enough to thread validated hint
  state through parser, executor, and `EXPLAIN` without introducing a global
  optimizer-hint DSL or precedence engine.
- **Index hints are safest as a constrained re-plan, not as a forced access
  path.** Re-planning against the named index and falling back when the
  predicate is incompatible preserves correctness while still honoring the
  hint whenever the current planner can legally use that index.
- **Hash-join hints should override only the threshold, never legality.**
  `HASH_JOIN` now bypasses `HASH_JOIN_MIN_ROWS`, but still relies on the
  existing equijoin detection and join-type support so the hint cannot invent
  unsupported hash-join semantics.

## 2026-04-22 - SQL CHECKPOINT (21.20)

- **`CHECKPOINT` is an admin SQL statement, not a WAL-rotation alias.** The
  SQL surface delegates to `TxnManager::checkpoint(storage)`, which wraps
  `Checkpointer::checkpoint(...)` only; WAL rotation remains a separate
  administrative path.
- **Checkpoint safety is enforced at the transaction manager boundary.**
  `TxnManager::checkpoint` shares the same "no active txns" contract used by
  WAL rotation, so every caller gets one authoritative guard instead of
  reimplementing `active_set` checks.
- **Implicit executor transactions must be bypassed explicitly.** The normal
  autocommit path opens a transaction before dispatch; administrative
  statements like `CHECKPOINT` need dedicated branches in `execute` and
  `execute_with_ctx` so they do not self-conflict.
- **Planner/cache dependencies for `CHECKPOINT` are empty, but it is still a
  mutating statement at the server gate.** It touches durability state, so the
  read-only/degraded-mode fast checks must treat it as mutating even though it
  has no table dependencies.

## 2026-04-21 - SQL cursors (21.10)

- **21.10 is SQL-session state, not a wire cursor feature.** `DECLARE`,
  `FETCH`, and `CLOSE` live entirely in `SessionContext`; MySQL
  `COM_STMT_FETCH` remains unsupported and intentionally separate from this
  implementation.
- **Materialize at `DECLARE`, slice at `FETCH`.** Cursor queries execute once,
  immediately, and persist `QueryResult::Rows` as `SessionCursor { columns,
  rows, pos }`, which keeps fetches O(k) over returned rows and avoids pinned
  executor state.
- **Cleanup belongs to transaction and connection boundaries.** Cursor state is
  cleared on `COMMIT`, `ROLLBACK`, implicit full-transaction rollback paths,
  reset-connection, and change-user handling so stale session state cannot leak
  across lifecycle boundaries.
- **Planner/cache dependencies come from the declared query, not the cursor
  command.** `DeclareCursor` contributes the inner query's table dependencies;
  `FetchCursor` and `CloseCursor` are session-local and dependency-free.
- **Wire smokes that run with `autocommit=False` must close setup transactions
  before explicit cursor `BEGIN`.** The harness itself can enter an implicit
  transaction via setup DDL/DML, so cursor protocol smokes need an explicit
  `COMMIT` before testing `BEGIN`.

## 2026-04-21 - Expression indexes (21.8)

- **Expression indexes are catalog-first metadata.** `IndexColumnDef.expr`
  stores canonical SQL text per indexed column, so ordinary B-Tree metadata
  can describe plain, partial, and expression indexes without a parallel
  storage format.
- **Compile once, evaluate everywhere.** CREATE INDEX build paths and shared
  DML index-maintenance paths parse stored expression SQL one time and then
  evaluate the resolved `Expr` against each row, which keeps heap and
  clustered maintenance semantics aligned.
- **Planner matching is normalized-SQL plus predicate implication.**
  Expression lookup/range planning still starts from normalized expression SQL
  equality, but partial expression indexes must additionally pass
  `predicate_implied_by_query(...)` using the full query `WHERE`.
- **Partial-expression matching must recurse through `AND`, not stop at the
  top-level predicate.** The usable expression may live on one branch while
  the other branch supplies the predicate implication needed for the partial
  index, so the planner walks subclauses while preserving the full filter as
  implication context.

## 2026-04-21 - TEMP and UNLOGGED tables (21.7)

- **Persistence is catalog metadata, not a separate executor branch.**
  `TablePersistence::{Permanent, Temporary, Unlogged}` is threaded through
  CREATE/LIKE/CTAS and stored in `TableDef`, so all later visibility and
  recovery logic can branch on one stable catalog field.
- **Session-local TEMP isolation is namespace-based.** TEMP tables live in a
  hidden per-session schema that is prepended to `search_path`, which lets
  ordinary name resolution shadow `public` without inventing TEMP-specific DML
  or DDL code paths.
- **Lookup schema and create-target schema must stay separate.**
  `SessionContext.default_create_schema()` exists because once TEMP prefixes
  `search_path`, unqualified lookup should hit TEMP first but ordinary
  permanent `CREATE TABLE` must still target `public` unless TEMP was
  explicitly requested.
- **TEMP cleanup belongs to connection lifecycle boundaries.** Reset,
  change-user, and disconnect all funnel through one cleanup helper that drops
  tables from the session temp schema after transaction rollback, then clears
  the temp-schema token from session state.
- **UNLOGGED semantics are enforced at open time, not per write.**
  `MmapStorage` writes a conservative clean-shutdown flag in page 0; dirty-open
  detection in `SharedDatabase::open_with_config` truncates only UNLOGGED
  tables before the server starts accepting queries.

## 2026-04-21 - Exclusion constraints (21.6b)

- **Owned-helper model:** 21.6b does not add a new row-vs-row exclusion
  engine. `EXCLUDE USING btree (... WITH =)` compiles to a real catalog
  constraint plus an owned backing UNIQUE index, so heap and clustered write
  paths reuse existing duplicate enforcement.
- **Catalog trailer stays append-only:** `ConstraintDef` now distinguishes
  `ConstraintKind::{Check, Exclusion}` and stores `owned_index_id` plus
  `(col_idx, operator)` exclusion elements in an optional trailer. Legacy CHECK
  rows with no trailer still decode as CHECK.
- **Error translation sits at shared write boundaries:** helper-index
  `UniqueViolation`s are translated back to `ExclusionViolation { table,
  constraint }` in shared executor entrypoints so INSERT/UPDATE-like paths and
  legacy execute flows all surface the same user-facing error.
- **Owned helper indexes are metadata, not user indexes:** `DROP INDEX` rejects
  direct removal of exclusion-owned helpers, `DROP CONSTRAINT` cleans both
  catalog objects, and `information_schema` filters helper UNIQUE metadata out
  of ordinary unique-constraint reporting.
- **Schema cloning must remap ownership:** `CREATE TABLE ... LIKE` now copies
  exclusion constraints together with their cloned helper indexes and rewrites
  `owned_index_id` to the new table-local helper index ids.

## 2026-04-21 - Generated columns (21.5f)

- **AST contract:** generated columns live as
  `ColumnConstraint::Generated { expr, kind }`; no separate `ColumnDef` field
  was added on the SQL AST side, so parser/analyzer/executor match sites keep
  using the existing constraint loop.
- **Catalog persistence:** `axiom_columns` now stores `generated_expr` and
  `generated_stored`; flag bit6 means expression bytes are present after
  `on_update_expr`, and bit7 differentiates `STORED` vs `VIRTUAL` without
  breaking older rows.
- **Validation boundary:** `execute_create_table` owns semantic validation for
  generated expressions. The parser only captures syntax; DDL rejects
  self-reference, generated-to-generated dependencies, unknown columns,
  subqueries, aggregates, and incompatible column attributes.
- **Write-time contract:** `executor/insert_helpers.rs::materialize_generated_columns`
  is the single recomputation point. INSERT paths call it after
  defaults/auto_increment; UPDATE paths call it after `SET` + `ON UPDATE`
  expressions; CHECK/FK/index maintenance/RETURNING all see the final
  recomputed row.
- **Out-of-scope forms are rejected early:** `VIRTUAL` generated columns and
  `ALTER TABLE ... GENERATED` are surfaced as explicit `NotImplemented` so no
  read-time synthesized column path leaks into the executor yet.

## 2026-04-13 — LATERAL-correlated JSON_TABLE (11.20d3)

- **Correlation detector**: `jsontable_is_correlated(&jt)` returns true
  when `doc_has_column_refs(doc)` or any PASSING expression has column
  refs. Single predicate reused by the join dispatcher and the
  first-FROM guard.
- **Analyzer fix**: join-side `FromClause::JsonTable` was never routed
  through `resolve_json_table` (only first-FROM was). 11.20d1 got away
  with it because PASSING literals resolve to themselves. 11.20d3
  requires real binding of outer column refs, so both `jt.doc` and
  every `jt.passing.iter_mut()` expression now run through
  `resolve_expr_full(ctx, outer_scopes)`.
- **Executor split**: `execute_select_with_joins_first_materialized`
  adds `correlated_jt: Vec<Option<JsonTableSpec>>` parallel to
  `scanned`. Non-correlated JT → `None`, materialize once as before.
  Correlated JT → `Some(spec)`, `scanned[i] = Vec::new()`. The
  combine loop consults the tracker and dispatches to
  `apply_correlated_jt_join` instead of `apply_join`.
- **Per-outer-row helper** `apply_correlated_jt_join` lives in
  `joins.rs`. It never builds a full right-set — for each outer row it
  evaluates `doc`, materializes, tests ON, and collects. INNER /
  CROSS APPLY / CROSS JOIN emit only matched rows; LEFT JOIN / OUTER
  APPLY NULL-pad unmatched; RIGHT JOIN / FULL JOIN return
  `NotImplemented` at the top of the function (PG-compatible
  rejection — correlated outer re-scan is semantically undefined).
- **Hash/spill optimizations not applied** to correlated JT — they
  require the full right set pre-built. Correlated JT is always
  nested-loop. Acceptable: the outer loop is already O(|outer|) and
  JT cardinality per outer row is bounded.
- **`LATERAL` keyword** is a pure parse-time no-op eaten at the start
  of `parse_from_item`. Covers `FROM LATERAL X`, `JOIN LATERAL X`, and
  `LATERAL (SELECT ...)` uniformly. No AST variant change; it is
  discarded because JSON_TABLE's lateral semantics are always implicit
  after 11.20d3, and bare-subquery LATERAL would require a different
  analyzer refactor.
- **First-FROM correlated doc** now raises a permanent
  `ParseError: correlated JSON_TABLE requires an outer FROM source`
  (the earlier 11.20d3 placeholder). The guard uses the same
  `jsontable_is_correlated` predicate, so PASSING outer refs are
  caught just like doc outer refs.

## 2026-04-13 — JSON_TABLE first FROM + CROSS/OUTER APPLY (11.20d2)

- **Join-loop split.** `execute_select_with_joins_ctx` (the ctx-path
  entry used when `FROM` is a `TableRef`) is now a thin wrapper that
  resolves the base table, scans it, and delegates to a new shared
  helper `execute_select_with_joins_first_materialized(stmt,
  first_source: JoinSourceSchema, first_rows: Vec<Row>, exec_ctx,
  conn_txn, ctx)`. The helper owns the entire nested-loop join
  pipeline that used to be inline. No semantic change for existing
  callers.
- **JSON_TABLE as first FROM + JOINs** now flows through the same
  helper: `execute_select_json_table_source` compiles JSON_TABLE,
  evaluates `doc` once against an empty row, materializes, and
  delegates — with a temp `ExecutionContext::new(storage, txn,
  &temp_bloom, None)` and `SessionContext::new()` built on the spot
  (same pattern as `execute_select_derived` for subquery-first FROM).
  The prior `NotImplemented` early-return is gone.
- **CROSS APPLY / OUTER APPLY as pure parse-time desugar.** New
  `Token::Apply` in the lexer. `parse_join_clauses` peeks two tokens
  to disambiguate `CROSS APPLY` from `CROSS JOIN`; the APPLY arm emits
  `JoinType::Inner` (or `Left` for `OUTER APPLY`) with
  `JoinCondition::On(Expr::Literal(Value::Bool(true)))`. No new
  `JoinType` variants — the downstream join loop, projection binder,
  and EXPLAIN output see standard `InnerJoin`/`LeftJoin` nodes.
- **`Outer` at top-level of `parse_join_clauses`** previously never
  matched (it is always consumed inside LEFT/RIGHT/FULL arms first).
  The APPLY dispatch is placed *above* the per-token match so
  `Outer + Apply` triggers before any other rule sees `Outer`; the
  `LEFT [OUTER] JOIN` path is untouched because `LEFT` consumes the
  `Outer` token inline.
- **Correlation guardrail unchanged.** `doc_has_column_refs` still
  rejects correlated `doc` on APPLY right-side sources with the
  11.20d3 `NotImplemented` message. The first-FROM path also runs it
  defensively even though a first-FROM `doc` cannot reference outer
  columns by definition.

## 2026-04-13 — `JSON_TABLE` multi-sibling + multi-level NESTED (11.20c)

- **Guards removed**: the 11.20b `depth >= 1 → NotImplemented` and
  `nested_count > 1 → NotImplemented` checks are gone from
  `compile_columns_recursive`. Replaced by a single defensive
  `depth > 32 → ParseError` (no real workload hits this).
- **Executor collapsed**: `materialize_json_table` + `fill_leaf_children`
  (11.20b) → single `emit_rows_rec(cols, node, template, level_ord, …)`.
  Body:
  1. Pass 1 — fill Regular / Ordinality / Exists into `template` in place.
  2. Collect NESTED siblings.
  3. If no NESTED → push `template`.
  4. Otherwise UNION: for each NESTED sibling, walk its child path;
     empty → push `template.clone()` (LEFT-OUTER pad); non-empty →
     recurse for each child match with `template.clone()` and the
     child's 1-based ordinality.
- Multi-level and multi-sibling share the same recursion; no plan-tree
  IR. PG's `JsonTableSiblingJoin` node becomes implicit via the sibling
  iteration.
- **Ordinality scope** is the `level_ord` argument; parent passes its
  parent-match index, nested recursion passes the child-match index.
  Resets happen naturally because `emit_rows_rec` is re-entered per
  child match.
- **LEFT-OUTER semantics unchanged**: template starts all-NULL;
  sibling-empty and inner-empty paths both express "don't modify this
  region" via cheap `template.clone()` pushes.

## 2026-04-13 — `JSON_TABLE` single-level `NESTED PATH` (11.20b)

- **AST** extended with `JsonTableColumn::Nested { path, columns }`. The
  parser `parse_column_def` dispatches on a leading `NESTED` ident before
  the generic identifier path, consumes an optional `PATH` keyword, reads
  the path literal, expects `COLUMNS`, and recurses into the same
  `parse_column_def` entry for the child list.
- **Spec compilation** is now a **DFS slot assignment**:
  - Each leaf column (`Regular`, `Ordinality`, `Exists`) carries a fixed
    `slot: usize` index in the emitted row.
  - Each `Nested` carries `slot_range: (usize, usize)` spanning its
    descendants; it contributes zero own slots.
  - `JsonTableSpec.total_slots` records the flat row width.
  - `compile_columns_recursive` enforces 11.20b scope at compile time:
    multi-sibling NESTED per list → `NotImplemented 11.20c`; depth ≥ 2
    → `NotImplemented 11.20c`. Unique column names are checked across
    every level. Per-level at-most-one `FOR ORDINALITY`.
- **Materialize**:
  - `materialize_json_table` allocates one `Vec<Value>` template of
    `total_slots` NULLs per parent match, fills leaves in place, and
    records the (at most one) NESTED column.
  - If no NESTED: push template as-is.
  - If NESTED present and child matches are empty: push template — slots
    in `slot_range` stay NULL → LEFT-OUTER pad.
  - If NESTED present and non-empty: clone template once per child,
    `fill_leaf_children` fills child-scope slots with a per-parent
    ordinality counter that resets to 1. Parent slots outside `slot_range`
    are preserved by the clone; no need to rewrite them.
- **`column_defs_for_ast`** is now recursive with an `inside_nested: bool`
  flag; columns beneath a NESTED are marked `nullable = true` because
  LEFT-OUTER pad can NULL-them at runtime.
- **Why MariaDB-style recursion over PG plan-tree**: PG
  (`parse_jsontable.c` + `nodeTableFuncscan.c`) flattens the tree into a
  `TableFunc` with a `SiblingJoin` plan node at planning time. We adopt
  MariaDB's recursive `scan_next` model (`sql/json_table.cc:322-361`)
  because the DFS slot layout maps directly onto it without a separate
  plan-tree IR. When 11.20c generalizes to multi-sibling / multi-level,
  the same recursion naturally extends — the gate in
  `compile_columns_recursive` is the single change.

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
