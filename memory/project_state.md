# Project State

## Current (2026-05-16)

**Active phase:** Phase 24 — Complete Type System
**Active subphase:** 24.1c — GENERATED ALWAYS AS IDENTITY (plan written, implementation pending)

**Strategic direction:** EMBEDDED-FIRST. Goal is to ship an embedded library release (like SQLite)
before a server release. All priorities below are set accordingly. See `memory/project_embedded_release.md`.

**Last verified gates:** Phase 24.3 closeout passed 4338/4338 tests, clippy clean, fmt clean, 639/652 wire pass.

**Recently completed:** 24.1 — TINYINT/SMALLINT/BIGSERIAL. 24.1b — SERIAL/SMALLSERIAL. 24.2 — REAL vs DOUBLE. 24.3 — Exact DECIMAL (2026-05-16).

---

## 🔴 HIGH PRIORITY — Embedded Release Blockers

These phases/subphases MUST be completed before the embedded library release:

### Phase 19.1 — Auto-vacuum background task
**Status:** ⏳ pending
**Why it blocks embedded:** MVCC dead tuples accumulate without periodic VACUUM. Long-running embedded
processes will run out of disk space. This is the ONLY functional gap for embedded mode.
**Scope:** background tokio task that wakes every N seconds, calls `vacuum()` on idle connections,
configurable via `axiomdb.toml`. No server wire protocol changes needed.

### Phase 24 — Type System Completion (remaining)
**Status:** 🔄 in progress
**Why it blocks embedded:** embedded users expect full SQL type support.
Priority order:
- [ ] 24.1c — GENERATED ALWAYS AS IDENTITY (plan written, implement NOW)
- [ ] 24.4 — CITEXT (case-insensitive text)
- [ ] 24.5 — BYTEA (binary data alias)
- [ ] 24.7 — TIMESTAMPTZ (timezone-aware timestamps)
- [ ] 24.8 — INTERVAL (time intervals)

### Embedded Packaging + Documentation
**Status:** ⏳ not started
**Why it blocks embedded:** users cannot adopt without installation path and docs.
- [ ] Python wheel (PyPI): `pip install axiomdb`
- [ ] npm package: `npm install axiomdb`
- [ ] `README.md` for embedded mode with quickstart
- [ ] API docs (Rust `cargo doc`, Python docstrings, JS JSDoc)
- [ ] VACUUM / memory management documentation
- [ ] Version tag: `v0.5.0-embedded-alpha`

---

## 🟡 MEDIUM PRIORITY — Nice-to-have for embedded

These improve the embedded experience but are NOT release blockers:

- Phase 25 — Type Optimizations (better performance for types already working)
- Phase 26 — Full ICU Collation (string ordering edge cases)
- Phase 27 — Real Query Optimizer (DP planner) — current planner is good enough
- Phase 28-30 — SQL Pro + Advanced Functions

---

## 🔵 LOW PRIORITY — Server-only, DEPRIORITIZED

Do NOT work on these until after the embedded release ships:

- **Phase 16** — Stored procedures, server-side pooler → server-only
- **Phase 17** — TLS, RBAC, RLS, audit, TDE → irrelevant for in-process embedded
- **Phase 18** — Streaming replication, PITR → server HA, not needed for embedded
- **Phase 19.2–19.21** — pg_stat_*, Prometheus metrics, health checks → server observability
- **Phase 22** (remaining) — REST/GraphQL/OData APIs → server-only
- **Phase 23** — Platform compat → deprioritize
- **Phase 31-35** — Final polish, deploy infra → post-server

---

## Phase subphase status

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
| 11.18a / 11.18b / 11.18c JSONB operators | ✅ closed |
| 11.19a / 11.19b / 11.19c SQL/JSON query functions | ✅ closed |
| 11.20a `JSON_TABLE` flat | ✅ closed |
| 11.20b `JSON_TABLE` single-level NESTED | ✅ closed |
| 11.20c `JSON_TABLE` multi-sibling + multi-level NESTED | ✅ closed |
| 11.20d1 `JSON_TABLE` WRAPPER / QUOTES / PASSING | ✅ closed |
| 11.20d2 `JSON_TABLE` first FROM + CROSS/OUTER APPLY | ✅ closed |
| 11.20d3 `JSON_TABLE` LATERAL-correlated doc + PASSING | ✅ closed |
| **11.20d4** `JSON_TABLE` as UPDATE/DELETE source | ✅ closed — **Phase 11.20 COMPLETE** |
| 11.21a–h JSONPath parity + planner pushdown | ✅ closed |
| 11.22a / 11.22b JSONB mutations | ✅ closed |
| 11.23a / 11.23b / 11.23d / 11.23e / 11.23f JSON Schema | ✅ closed |
| 11.24a / 11.24b / 11.24d (partial) Oracle JSON | ✅ closed |
| **11.25** JSON SRF + aggregates + construction helpers | ✅ complete |

### Phase 20 subphase status

| Subphase | Status |
|---|---|
| 20.1 Regular views | ✅ closed |
| 20.2 Sequences | ✅ closed |
| 20.3 ENUMs | ✅ closed |
| 20.4 Arrays | ✅ closed |
| **20.5 COPY FROM/TO** | ✅ closed |
| 20.5b SELECT INTO OUTFILE | ✅ closed |
| 20.6 Parquet READ_PARQUET + COPY TO PARQUET | ✅ closed |
| **20.7 Incremental backup** | ✅ closed |
| **20.8 COPY streaming** | ✅ closed |
| **20.15 Regex operators** | ✅ closed |
| **20.12 ORDER BY RANDOM()** | ✅ closed |
| **20.11 TABLESAMPLE** | ✅ closed |
| 20.10 GENERATE_SERIES | ✅ closed |
| 20.14 UNNEST in SELECT list | ✅ closed |

### Completed phases

Phases 1–10, 22b, 39, 40 — all closed. See `docs/progreso.md` for subphase details.

### Key open gaps (deferred)

- Phase 4: 4.22f (DROP PRIMARY KEY on clustered — requires full rebuild)
- Phase 39: MVCC version chains, root checkpoint persistence, FK clustered children, separator overflow, 39.19b uncommitted

---

## GitHub

Account: `lordmacu` (`lordmacu@users.noreply.github.com`). Push target: `origin main`.
