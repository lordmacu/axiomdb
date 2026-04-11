# Spec: 11.17 GIN Index for JSONB

## What to build (not how)

A GIN (Generalized Inverted Index) for JSONB columns that accelerates the `@>`
(containment) operator. When a JSONB column has a GIN index and a query uses
`col @> '{"key":"val"}'`, the planner uses the index instead of a full table scan.

**Reference:** PostgreSQL `src/backend/utils/adt/jsonb_gin.c` (jsonb_ops strategy),
`src/backend/access/gin/ginget.c` (term-based lookup + RID intersection).

## Inputs / Outputs

**CREATE INDEX:**
```sql
CREATE INDEX idx_data_gin ON docs USING gin (data);
-- current implementation extracts terms from JSONB, JSON, or text values that
-- contain valid JSON; stricter DDL rejection for non-JSON columns is a follow-up
```

**Query with index:**
```sql
-- Containment: find all docs whose JSONB contains {"status":"active"}
SELECT * FROM docs WHERE data @> '{"status":"active"}';

-- Nested containment
SELECT * FROM docs WHERE data @> '{"user":{"role":"admin"}}';

-- Array containment
SELECT * FROM docs WHERE data @> '[1,2]';

-- Fallback (no GIN index): JSON_CONTAINS still works via full scan
SELECT * FROM docs WHERE JSON_CONTAINS(data, '{"a":1}');
```

**Operator behavior:**
- `doc @> query` → 1 if `query` is a subset of `doc`, else 0
- `NULL @> x` → NULL; `x @> NULL` → NULL
- Works on both `JSON` and `JSONB` columns

## Term encoding (PostgreSQL jsonb_ops-inspired)

Each document is decomposed into a set of **terms**. A term is a byte string:

```
[flag: u8][payload: bytes]
```

Flags (same as PostgreSQL JGINFLAG_*):
- `0x01` KEY   — object key string, or string-typed array element
- `0x02` NULL  — null value (payload: empty)
- `0x03` BOOL  — boolean (payload: 1 byte, 0=false/1=true)
- `0x04` NUM   — numeric/real (payload: canonical decimal string bytes)
- `0x05` STR   — string value that is NOT an object key

Term extraction from a document (DFS, all levels):
- Object key   → term `[0x01][key_utf8_bytes]`
- Object value → term for the value's type
- Array element string → term `[0x01][element_utf8_bytes]` (PostgreSQL compat: string array elements treated as keys)
- Array element non-string → term for the element's type

B-Tree key format for each heap-table GIN entry:
```
[term_bytes][0x00][page_id: 8 LE][slot_id: 2 LE]
```
The `0x00` null byte separates term from RID, enabling range scan by term prefix.

B-Tree key format for clustered-table GIN entries:
```
[term_bytes][0x00][encoded_primary_key]
```
The B-Tree value for clustered entries is the dummy RID `(0, 0)`. The real
bookmark is the encoded primary-key suffix, which the executor uses to look up
the clustered row before applying the structural containment recheck.

## Use cases

1. Filtering a catalog table by JSONB tags
2. Multi-tenant row filtering where tenant config is stored as JSONB
3. Product attribute queries: `WHERE attrs @> '{"color":"red","size":"M"}'`
4. Log analysis: `WHERE event @> '{"level":"error"}'`

## Acceptance criteria

- [x] `CREATE INDEX ... USING gin (col)` on JSONB column succeeds
- [x] `@>` operator parsed from SQL, evaluates correctly without index (full scan)
- [x] `@>` with GIN index → planner chooses `AccessMethod::GinScan`; full scan otherwise
- [x] INSERT: GIN terms for new row are inserted into the B-Tree index
- [x] DELETE: GIN terms for deleted row are removed from the B-Tree index
- [x] UPDATE: old terms deleted, new terms inserted
- [x] `WHERE data @> '{"a":1}'` returns correct rows (integration test)
- [x] Nested object containment works, including false-positive term recheck
- [x] Nested array containment inside object payloads works with GIN candidates
- [x] Docs with no matching terms are not returned after structural recheck
- [x] Clustered tables use encoded primary-key bookmarks instead of heap RIDs
- [x] `rtk cargo test -p axiomdb-sql --test integration_jsonb` passes
- [x] `rtk cargo test -p axiomdb-sql` passes
- [x] Local bench scenario `jsonb_gin_contains` exists and returns explicit match counts
- [ ] DDL validation rejects non-JSON columns instead of creating an empty/useless GIN index
- [ ] Wire smoke: CREATE INDEX USING gin + `@>` query returns correct rows

## Out of scope

- `jsonb_path_ops` (hash-based path ops) — deferred to Phase 11.21
- `@?` (JSONPath existence) and `@@` (JSONPath match) operators — deferred to Phase 11.21
- GIN fast-update pending list (PostgreSQL ginfast.c) — deferred; direct insert is correct
- Multi-column GIN indexes — deferred to Phase 30.1; only single JSON/JSONB column supported now
- VACUUM for GIN stale entries — inherits B-Tree delete; no separate cleanup needed

## Dependencies

- `crates/axiomdb-types/src/jsonb.rs` — `JsonbRef`, `jsonb_contains()`, `object_iter()`, `array_iter()`, GIN term extraction
- `crates/axiomdb-sql/src/expr.rs` — `BinaryOp::JsonContains`
- `crates/axiomdb-sql/src/ast.rs` — `IndexType::Gin`
- `crates/axiomdb-sql/src/parser/expr.rs` — `@>` token → `JsonContains`
- `crates/axiomdb-sql/src/parser/ddl.rs` — `"gin"` → `IndexType::Gin`
- `crates/axiomdb-sql/src/executor/ddl_create_index.rs` — `IndexType::Gin → 4`
- `crates/axiomdb-sql/src/index_maintenance.rs` — heap GIN key helpers, DML maintenance, delete helpers, root persistence
- `crates/axiomdb-sql/src/executor/insert_helpers.rs` — clustered-table GIN insert maintenance
- `crates/axiomdb-sql/src/executor/update_clustered_helpers.rs` — clustered-table GIN update/delete maintenance
- `crates/axiomdb-sql/src/planner_types.rs` — `AccessMethod::GinScan` variant
- `crates/axiomdb-sql/src/planner_select.rs` — detect `col @> literal` with GIN index
- `crates/axiomdb-sql/src/executor/select_helpers.rs` — execute heap and clustered `GinScan`
- `crates/axiomdb-sql/src/executor/select_core.rs` / `select_ctx.rs` — route `GinScan`
- `crates/axiomdb-sql/src/executor/exec_explain.rs` — render `GinScan` in EXPLAIN
- `crates/axiomdb-sql/src/eval/ops.rs` — evaluate `BinaryOp::JsonContains`
- `crates/axiomdb-sql/tests/integration_jsonb.rs` — JSONB/GIN regression tests
