# Lessons Learned

## 2026-04-22 - Phase 21.23

- **Acceptance suites should target interactions, not restate feature-unit
  coverage.** The value of `21.23` came from composing CTEs, `MERGE`,
  savepoints, cursors, `CHECKPOINT`, and grouping sets in shared workflows,
  not from reasserting every parser edge already tested elsewhere.
- **A stale roadmap line can pull the implementation in the wrong direction.**
  `21.23` still mentioned window functions in `docs/progreso.md`, but the repo
  has no SQL `OVER (...)` support yet; fixing the wording first prevented a
  test-only subphase from silently turning into feature expansion.
- **Wire smokes with `autocommit=False` are easy to invalidate with local
  setup writes.** If a block does DDL/DML setup and then starts an explicit
  transaction, it must commit or rollback its own implicit transaction first
  or the smoke will fail for harness-state reasons.

## 2026-04-22 - Phase 21.11

- **The visible SQL feature was blocked by lexer behavior, not by planner code.**
  Until `/*+ ... */` survived tokenization, every discussion about hinted
  access paths was premature.
- **A narrow, effectful hint MVP is better than a large fake compatibility
  surface.** Shipping `INDEX`, `HASH_JOIN`, and advisory `PARALLEL` with real
  semantics closed the user-visible gap without taking on MariaDB's full hint
  precedence matrix.
- **Hinted plans need explicit guard rails against over-forcing.**
  Re-planning on the named index and falling back cleanly is safer than
  blindly forcing a path the current predicate cannot support.

## 2026-04-22 - Phase 21.20

- **Administrative SQL can break if it reuses ordinary autocommit blindly.**
  `CHECKPOINT` looked trivial until the executor wrapped it in an implicit
  transaction and caused a false `TransactionAlreadyActive` against itself.
- **Checkpoint and WAL rotation are different operational contracts.**
  Exposing SQL `CHECKPOINT` cleanly meant reusing only the durable-checkpoint
  path, not smuggling in file rotation or truncation side effects.
- **The wire harness is the right place to catch admin-statement lifecycle
  drift.** A simple `CHECKPOINT` OK + explicit-txn rejection smoke protects
  the user-visible contract better than parser or unit tests alone.

## 2026-04-21 - Phase 21.10

- **SQL cursors were the right cut; streaming portals were not.** A
  materialized-session MVP closed the user-visible `DECLARE` / `FETCH` /
  `CLOSE` gap without entangling executor suspension, snapshot lifetime, or
  MySQL prepared-statement cursor semantics.
- **Lifecycle cleanup has to be centralized, not statement-specific.** Cursor
  leaks would have shown up on `COM_RESET_CONNECTION` and `COM_CHANGE_USER`
  long before ordinary SQL tests caught them, so cleanup needed to live on the
  shared transaction/session boundary paths.
- **Wire harnesses with `autocommit=False` accumulate implicit transaction
  state between blocks.** A new explicit-transaction smoke can fail even when
  the engine is correct unless the setup writes are committed before `BEGIN`.

## 2026-04-21 - Phase 21.8

- **A closed-looking feature can still hide a planner gap.** Expression
  indexes were already implemented across parser/catalog/executor, but the
  planner still rejected the partial-expression case until we audited the
  matcher end to end.
- **Partial expression indexes need two different predicates at once.** One
  subexpression proves "this index key is relevant" (`LOWER(email) = ...`);
  the full `WHERE` proves "the partial predicate is satisfied"
  (`active = TRUE`). Treating those as the same expression loses valid plans.
- **`EXPLAIN` on tiny tables is a poor oracle for planner matcher tests.**
  Cost gating can legitimately choose `ALL` even when the matcher works, so
  the stable coverage for partial expression indexes belongs in planner unit
  tests, not in small-table EXPLAIN assertions.

## 2026-04-21 - Phase 21.7

- **TEMP tables fit best as a namespace problem, not a storage problem.**
  Reusing hidden schemas plus normal resolution avoided a parallel executor
  path and made TEMP shadowing/drop behavior line up with existing catalog and
  DDL machinery.
- **Once TEMP prefixes lookup, creation needs its own default-schema rule.**
  Using `search_path[0]` for both lookup and target schema silently redirects
  permanent creates into the temp namespace; splitting
  `default_create_schema()` from lookup resolution keeps the semantics sane.
- **Dirty-open cleanup is safer when the storage layer is conservative.**
  Mark the database dirty as soon as it opens, only flip it back to clean on a
  graceful close, and let startup truncate `UNLOGGED` tables from that one
  signal instead of scattering per-table crash heuristics.

## 2026-04-21 - Phase 21.6b

- **Reuse owned UNIQUE enforcement before inventing a new exclusion engine.**
  For equality-only `EXCLUDE USING btree`, the helper-index approach closed the
  user-visible feature with far less risk than a bespoke row scan / operator
  dispatcher, while still preserving proper constraint semantics at the SQL
  surface.
- **If an internal helper is catalog-owned, protect every lifecycle edge.**
  The first implementation pass is not enough unless `DROP INDEX`, `DROP
  CONSTRAINT`, `CREATE TABLE ... LIKE`, and metadata views all understand that
  the helper index is internal state owned by the constraint.
- **Translate internal storage errors at shared executor boundaries.** UNIQUE
  enforcement already fires in multiple DML paths; the reliable fix is a shared
  translation layer that maps owned-helper duplicates back to exclusion
  violations before the error escapes to sessions or wire clients.

## 2026-04-21 - Phase 21.5f

- **Generated-column semantics must live in one helper, not in each DML arm.**
  Heap INSERT, clustered INSERT, UPDATE, `ON CONFLICT`, ODKU, `MERGE`, and
  UPDATE JOIN all need the same recomputation order. Centralizing it in
  `materialize_generated_columns()` prevented subtle divergence between
  "normal" and conflict-update paths.
- **Persist the expression as SQL text first; optimize later.** Storing
  `generated_expr` in the catalog and re-parsing it at write time keeps the
  on-disk format simple and backward-compatible while still letting every path
  reuse the normal expression evaluator.
- **Treat explicit writes as DEFAULT-or-error centrally.** Once INSERT/UPDATE
  use one "generated columns cannot be assigned explicitly" guard, positional
  INSERT arity, `DEFAULT`, and update-like paths become predictable instead of
  each path inventing its own rule.

## 2026-04-13 — Phase 11.20b

- **Slot layout up front is easier than deferred row assembly.** First
  draft of the NESTED executor rebuilt the row incrementally via
  `Vec::push`; slot collisions between parent leaves and the NESTED
  expansion made the logic fragile. Switching to a fixed DFS slot
  assignment (every leaf knows its slot, every `Nested` owns a
  `slot_range`) made `materialize` a straight "clone template, overwrite
  range" — and 11.20c's multi-level case becomes the same shape.
- **LEFT-OUTER is cheapest as "don't modify the template".** Because the
  template starts as all-`Null`, an empty child match means we push the
  template as-is; no NULL-initialisation pass needed.
- **Recursive name uniqueness beats per-level.** Enforcing unique column
  names *across all levels* up front (`collect_names_recursive` before
  slot assignment) surfaces duplicates as clean parse errors rather than
  as surprising JOIN ambiguity at bind time.

## 2026-04-13 — Phase 11.20a

- **Parser dispatch for table-valued functions must check the `(` to keep
  the identifier namespace usable.** `JSON_TABLE` followed by `(` commits
  to the table-function branch; without `(` the parser falls through to
  the regular table-ref path so a real table named `json_table` still
  resolves. Implementation uses `peek_at(1) == Token::LParen`; no rollback
  needed since the ident hasn't been consumed yet.
- **`eat_ident_ci("DEFAULT"|"FOR"|"EXISTS"|"TRUE"|"FALSE"|"NULL")` silently
  fails when those are reserved tokens.** The helper only matches
  `Token::Ident` / `Token::QuotedIdent`. Every parser that wants to accept
  one of these SQL keywords must use the dedicated `Token::*` variant or
  fall back to `eat(&Token::Default)` etc. — caught this only at test time
  because the "ON EMPTY / ON ERROR" clauses silently no-op'd.
- **LATERAL semantics are an executor-architecture constraint, not a
  parser one.** `JSON_TABLE(u.tags, ...)` joined against `u` requires
  per-left-row re-materialization of the right source. The current
  `select_joins_ctx` pre-scans every source once into `Vec<Row>` before the
  nested-loop combine — so we gate correlated `doc` with
  `doc_has_column_refs` and raise `NotImplemented`. Refactoring to a
  generic per-left-row right-source callback belongs in the 11.20d
  follow-up; trying to sneak it into 11.20a would have entangled subquery
  JOIN arms, DML JOINs, and the legacy `select_helpers` path all at once.
- **AxiomDB has two JSONPath step enums.** `eval::functions::json::PathStep`
  carries filter `Expr` nodes and is non-trivially cloneable; JSON_TABLE
  doesn't need filter-in-path support at 11.20a and uses its own
  `PathStepOwned` (pure data, `Clone`). Keeps the module self-contained and
  lets `JsonTableSpec` live in executor memory without borrowing the AST.

## 2026-04-10 — Phase 11.x

- **TOAST refcounts belong on the owned BLOB chain, not clustered overflow.** Use versioned `ABOB` header only for TOAST/BLOB-owned chains; clustered row overflow stays non-refcounted so Phase 39 physical descriptors remain stable.
- **Buffer pool eviction: scan LRU candidates at most once per attempt.** If all candidates are pinned, exit; don't spin. Resume enforcement when a page is unpinned.
- **Large wire values → use `COM_STMT_SEND_LONG_DATA`, not `COM_QUERY` literals.** `COM_QUERY` stack-overflows tokio worker around 7 KB. Separate BLOB wire correctness from long-literal parser hardening.
- **`decode_row_masked` must detect TOAST sentinels before skipping variable payloads.** Read u24, check for sentinel, consume 12-byte pointer; don't interpret `0xFF_FFFE` as inline payload length.
- **Split roadmap entries when a feature combines a usable scope + deferred storage work.** For 11.4: text-backed Native JSON; JSONB layout and GIN indexing explicitly deferred to 11.16/11.17.

## 2026-04-09 — B-tree safe descent (Phase 39)

- **Delete safe descent needs stricter predicate than insert.** Insert: "child has room for one more key" sufficient. Delete: must also guarantee child won't enter CoW rebalance path. Simplest: child is leaf with > MIN_KEYS_LEAF keys.
- **Clustered B-tree never allocates new pid on rebalance** (left page reused); safe-descent predicate is purely byte occupancy.
- **`cargo test --workspace` is the correct final gate command.** Linux is the faster validation baseline.

## 2026-04-03 — Clustered maintenance (Phase 39)

- **Heap→clustered migration contract:** build new clustered root first, flush before any catalog pointer change, swap metadata, queue old pages through deferred free (never inline `free_page` during swap).
- **Clustered VACUUM predicate:** use physical row existence, not snapshot visibility, to decide secondary bookmark cleanup.

## 2026-04-02 — Clustered storage invariants (Phase 39)

- **Clustered undo keys = PK bytes + exact row image.** Never `(page_id, slot_id)` — clustered pages defragment and relocate rows freely.
- **Tombstone-reuse INSERT → log as clustered update undo, not insert undo.** Rollback must restore the old tombstone.
- **Clustered SQL UPDATE must capture exact old row image before mutation.** Secondary undo needs both halves: delete new bookmark + reinsert old bookmark.
- **Variable-size clustered split by encoded bytes, not key count.** Left page keeps old ID; only right sibling allocated.
- **Clustered overflow: keep PK and RowHeader inline, spill only row tail.** Not a full TOAST — no compression, no generic BLOB references in Phase 39.
- **Clustered secondary key = secondary_logical_key ++ missing_PK_columns.** Never depends on heap RecordId.
- **Crash recovery: keep committed clustered roots separate from all-seen roots.** Rolled-back transactions must not poison the root map.
- **Covering IndexOnlyScan on clustered: normalize back to clustered-aware access** until a true index-only path exists. Visibility comes from clustered row header, not heap slot.

## 2026-03-29 — Test structure

- **Split integration tests by execution path, not size alone.** ~1000 lines is a watch signal, not an automatic trigger. Files around subphase boundaries produce the cleanest ownership.

## 2026-03-26 — Implementation process

- **Incremental validation:** start with `cargo test -p <touched-crate>`, expand to dependents only when public API, on-disk format, WAL/recovery, SQL semantics, or wire behavior changed.
- **Spec discipline:** read codebase files before writing the spec. List real files reviewed. Dependencies must name real files, not just modules.
- **Plan must have zero loose ends:** return types, error paths, ownership boundaries, exact call sites — all decided before implementation starts.
- **Large Rust module splits:** use `include!` in first step to preserve private visibility while improving reviewability.
- **Separate transport lifecycle from SQL session state.** `COM_RESET_CONNECTION` tests the boundary.
- **DML perf has two diagnoses:** candidate discovery vs index maintenance — profile both independently.
- **UPDATE index skip requires two conditions:** RID stable AND logical key / predicate membership unchanged.
- **Research citations:** cite exact file paths from `research/`, not just "PostgreSQL" or "MariaDB". Use borrow/reject/adapt mindset.
- **Critical subphases (correctness, durability, WAL, crash recovery):** review all relevant engines in `research/`, not just the obvious one. Correctness before speed.
- **Default choice:** pick best + most robust solution over quick/minimal hacks. If semantics unclear, consult `research/{postgres,mariadb,duckdb}` source and cite the exact path.
