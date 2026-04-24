# Project State

## Current (2026-04-23)

**Active phase:** Phase 13 — Advanced PostgreSQL
**Active subphase:** Phase 13.12 — `13.12` Statement-level triggers closed.
AxiomDB now supports a bounded statement-trigger MVP for base tables:
`CREATE TRIGGER ... AFTER INSERT|UPDATE|DELETE ... FOR EACH STATEMENT AS
SELECT ...`, `DROP TRIGGER ... ON table`, and `SHOW CREATE TRIGGER`. Trigger
definitions persist in table catalog metadata, fire once after top-level DML,
and abort the outer statement when the validation `SELECT` returns rows. The
MVP is intentionally narrower than full trigger support: `BEFORE`, `FOR EACH
ROW`, `WHEN`, `SIGNAL`, transition tables, recursive triggers, and procedural
bodies remain deferred to Phase 16.

**Last verified gates:** `cargo fmt --check`; `cargo test -p axiomdb-sql --test integration_ddl_parser --test integration_statement_triggers`; `python3 tools/wire-test.py`; `cargo test --workspace`; `cargo clippy --workspace -- -D warnings`.

**Next:** Phase 13.7 / 13.8 remain superseded by Phase 40 lock-manager work; the next actionable Phase 13 subphase is 13.13 Collation system.

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

### Completed phases

Phases 1–10, 22b, 39, 40 — all closed. See `docs/progreso.md` for subphase details.

### Key open gaps (deferred)

- Phase 4: 4.10f, 4.11b, 4.4g, 4.22f, 4.22e
- Phase 5: 5.2d, 5.5b
- Phase 6: 6.5b, 6.6d, 6.6c
- Phase 39: MVCC version chains, root checkpoint persistence, FK clustered children, separator overflow, 39.19b uncommitted

### GitHub

Account: `lordmacu` (`lordmacu@users.noreply.github.com`). Push target: `origin main`.
