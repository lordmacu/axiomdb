---
name: Embedded-first release strategy
description: Strategic decision to ship embedded library (SQLite-style) before server mode; priorities and what to deprioritize
type: project
---

Decision: Ship AxiomDB as an embedded library first (like SQLite/DuckDB), then server mode later.

**Why:** All foundations for embedded are done (Phases 1-10, MVCC, SQL, FFI bindings for Python/Node.js/C).
Embedded needs almost no additional work. Server mode needs TLS + auth + observability — 3-4 more weeks minimum.
Embedded-first gets something shippable now.

**How to apply:** When deciding what to work on next, ALWAYS prefer embedded-release tasks over server tasks.
Refuse to plan or implement server-only features (TLS, RBAC, replication, pg_stat_*) until after embedded ships.

## What is needed for the embedded release

### Functional gaps (must fix before release)
- **Phase 19.1 — Auto-vacuum**: Background task that periodically calls `vacuum()`. Without it, MVCC
  dead tuples accumulate indefinitely in long-running embedded processes. This is the ONLY blocker.

### Type completeness (high value for embedded users)
- Phase 24.1c — GENERATED ALWAYS AS IDENTITY (in progress, plan written)
- Phase 24.4 — CITEXT
- Phase 24.5 — BYTEA
- Phase 24.7 — TIMESTAMPTZ
- Phase 24.8 — INTERVAL

### Packaging (needed to ship)
- Python wheel on PyPI (`pip install axiomdb`)
- npm package (`npm install axiomdb`)
- README with embedded quickstart
- API docs + VACUUM documentation
- Release tag: `v0.5.0-embedded-alpha`

## What NOT to work on (server-only, deprioritized)

- Phase 16: stored procs, server pooler
- Phase 17: TLS, RBAC, RLS, TDE, audit — irrelevant for in-process use
- Phase 18: streaming replication, PITR
- Phase 19.2-19.21: pg_stat_*, Prometheus, health endpoints
- Phase 22 remaining: REST/GraphQL/OData APIs
- Phase 31-35: deploy infrastructure

## Already done (no work needed)

- Embedded `Db` struct + C FFI (Phase 10) ✅
- Python ctypes binding ✅
- Node.js koffi binding ✅
- MVCC + transactions ✅
- Full SQL (SELECT/INSERT/UPDATE/DELETE/DDL) ✅
- JSON, arrays, FTS, JSONB ✅
- B+Tree, secondary indexes, FK ✅
- In-memory mode (`:memory:`) ✅
