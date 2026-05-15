# Project State

## Current (2026-05-14)

**Active phase:** Phase 20 — Types + import/export
**Active subphase:** Phase 20.4 — Arrays COMPLETE.
Phase 20.4 added PostgreSQL-compatible SQL arrays with DDL (`INT[]`, `TEXT[]`, etc.),
ARRAY constructor (`ARRAY[1,2,3]`), subscript access (`arr[1]`), `@>` containment,
`&&` overlap, `||` concatenation, `=`/`<>` equality, GIN indexes, 17 array functions,
ANY/ALL quantifiers, and FROM UNNEST() set-returning function.

**Last verified gates:** Phase 20.4 closeout passed 7 integration test files
(139 tests: arrays, array_operators, array_functions, array_gin, array_agg,
array_any_all, array_unnest), wire smoke, `cargo test --workspace`,
`cargo clippy --workspace -- -D warnings`, and `cargo fmt --check`.

**Recently completed:** 22b.6 — FDW predicate pushdown (2026-05-14).
`extract_fdw_pushable` extracts `col = literal` predicates from WHERE, `render_fdw_url`
substitutes `{col}` path placeholders and appends `pushdown_cols` as `?k=v` plus
`limit_param`. Full WHERE always applied locally for correctness. 3837/3837 tests pass.
Phase 22b fully closed.

**Next:** Start Phase 20.5 COPY FROM/TO or Phase 21 Advanced SQL (CTE, MERGE, etc.)

### Phase 21 subphase status

| Subphase | Status |
|---|---|
| 21.2 Non-recursive CTEs | ✅ closed |
| 21.3 / 21.3b Recursive CTEs | ✅ closed |
| 21.4 / 21.4b RETURNING | ✅ closed |
| **21.5 MERGE / UPSERT** | ✅ closed |
| 21.5b-e MySQL DML variants and multi-table DML | ✅ closed |
| 21.5f GENERATED ALWAYS AS columns | ✅ closed |
| 21.6 CHECK constraints | ✅ closed |
| 21.6b Exclusion constraints | ✅ closed |
| 21.7 TEMP and UNLOGGED tables | ✅ closed |
| 21.8 Expression indexes | ✅ closed |
| 21.10 Cursors | ✅ closed |
| 21.20 CHECKPOINT | ✅ closed |
| 21.11 Query hints | ✅ closed |
| 21.23 Advanced SQL tests | ✅ closed |
| 21.24 ORM compatibility tier 2 | ✅ closed |
| 21.25 PIVOT dynamic | ✅ closed |
| 21.16 DEFERRABLE constraints | ✅ closed |

### Phase 11 subphase status

| Subphase | Status |
|---|---|
| 11.2d BLOB refcount | ✅ closed |
| 11.4 Native JSON | ✅ closed |
| 11.7 Advanced FTS (boolean, phrase, prefix) | ✅ closed |
| 11.8 Buffer Pool Manager | ✅ closed |
| 11.9 + 11.10 Prefetch + Write Combining | ✅ closed |
| 11.16 Binary JSONB + JSONPath | ✅ closed |
| 11.17 GIN index for JSONB | ✅ closed |
| 11.18a / 11.18b / 11.18c JSONB operators | ✅ closed — path/delete parity uses documented JSONB-array RHS divergence |
| 11.19a / 11.19b / 11.19c SQL/JSON query functions | ✅ closed |
| 11.20a `JSON_TABLE` flat | ✅ closed |
| 11.20b `JSON_TABLE` single-level NESTED | ✅ closed |
| 11.20c `JSON_TABLE` multi-sibling + multi-level NESTED | ✅ closed |
| 11.20d1 `JSON_TABLE` WRAPPER / QUOTES / PASSING | ✅ closed |
| 11.20d2 `JSON_TABLE` first FROM + CROSS/OUTER APPLY | ✅ closed |
| 11.20d3 `JSON_TABLE` LATERAL-correlated doc + PASSING | ✅ closed |
| **11.20d4** `JSON_TABLE` as UPDATE/DELETE source | ✅ closed — **Phase 11.20 COMPLETE** |
| 11.21a–h JSONPath parity + planner pushdown | ✅ closed — simple `@?` / `@@` key probes now use GIN with executor recheck |
| 11.22a / 11.22b JSONB mutations | ✅ closed |
| 11.23a / 11.23b / 11.23d / 11.23e / 11.23f JSON Schema | ✅ closed — 11.23c moved to `features-roadmap.md` (Oracle-only DDL) |
| 11.24a / 11.24b / 11.24d (partial) Oracle JSON | ✅ closed — 11.24c moved to `features-roadmap.md` (Oracle-only dot notation) |
| **11.25** JSON SRF + aggregates + construction helpers (PG streaming parity) | ✅ complete — 11.25a ✅ 11.25b ✅ 11.25c ✅ 11.25d ✅ |

### 11.20 follow-ups

- **11.20b** — Single-level `NESTED PATH` with LEFT-OUTER NULL padding + per-level ordinality.
- **11.20c** — Multi-sibling (`UNION` semantics) and multi-level NESTED PATH.
- **11.20d1** — WRAPPER/QUOTES, `PASSING` on the row path. ✅ closed.
- **11.20d2** — JSON_TABLE as first FROM + JOINs; CROSS/OUTER APPLY parser sugar. ✅ closed.
- **11.20d3** — LATERAL-correlated `doc` / PASSING referencing outer columns + `LATERAL` keyword. ✅ closed.
- **11.20d4** — JSON_TABLE as UPDATE/DELETE source (MERGE deferred until MERGE lands).

### Phase 20 subphase status

| Subphase | Status |
|---|---|
| 20.1 Regular views | ✅ closed |
| 20.2 Sequences | ✅ closed |
| 20.3 ENUMs | ✅ closed |
| **20.4 Arrays** | ✅ closed |
| 20.5 COPY FROM/TO | ⏳ pending |
| 20.14 UNNEST | ⏳ pending |

### Completed phases

Phases 1–10, 22b, 39, 40 — all closed. See `docs/progreso.md` for subphase details.

### Key open gaps (deferred)

- Phase 4: 4.10f, 4.11b, 4.4g, 4.22f, 4.22e
- Phase 5: 5.2d, 5.5b
- Phase 6: 6.5b, 6.6d, 6.6c
- Phase 39: MVCC version chains, root checkpoint persistence, FK clustered children, separator overflow, 39.19b uncommitted

### GitHub

Account: `lordmacu` (`lordmacu@users.noreply.github.com`). Push target: `origin main`.
