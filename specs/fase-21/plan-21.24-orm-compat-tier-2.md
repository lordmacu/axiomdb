# Plan: 21.24 — ORM compatibility tier 2

Phase: 21 — Advanced SQL
Task: 21.24 ORM compatibility tier 2
Spec: specs/fase-21/spec-21.24-orm-compat-tier-2.md
Status: done

## Summary

Treat `21.24` as a compatibility-audit-and-shims sprint, not as a vehicle for
shipping identity columns or deferred foreign keys. First identify the exact
Prisma / ActiveRecord-visible metadata and connect probes we can realistically
support with the current engine. Then add a bounded compatibility smoke plus
only the narrow parser / wire / metadata fixes required for that smoke.
Finally, close the subphase with explicit documentation of what still remains
blocked by `21.16` and `24.1c`.

## Dependencies

Must be done first:
- [x] `specs/fase-21/spec-21.24-orm-compat-tier-2.md` accepted.
- [x] Current wire intercept / SHOW support baseline understood.

Blocks:
- [x] Clean statement of Prisma / ActiveRecord tier-2 compatibility in Phase 21.

## Affected files

Likely modified files:
- `crates/axiomdb-network/src/mysql/handler_sql_intercept.rs` — add narrow
  ORM-visible metadata shims if still missing.
- `crates/axiomdb-sql/src/parser/*` and/or SHOW executors — only if an audited
  metadata form is missing from the SQL layer itself.
- `tools/wire-test.py` — compatibility smoke if wire-visible.
- `docs/progreso.md`, `docs/fase-21.md`, `memory/project_state.md`,
  `memory/architecture.md`, `memory/lessons.md` — closeout + incompat notes.

Likely new files:
- `specs/fase-21/spec-21.24-orm-compat-tier-2.md`
- `specs/fase-21/plan-21.24-orm-compat-tier-2.md`
- possibly one focused integration test file if the smoke belongs better in
  Rust than in the wire harness

## Step 1 — Audit the real ORM surface

**Goal:** pin down which Prisma / ActiveRecord probes matter for this tier.
**Files:** local docs / current code only.
**Approach:** inventory already-supported metadata queries vs obvious missing
ones, and choose the minimum viable tier-2 baseline.

### Audit targets

```text
- connect / handshake queries
- SHOW FULL TABLES / SHOW FULL COLUMNS / SHOW TABLE STATUS
- schema discovery expectations around auto-increment / SERIAL
- migration-visible use of RETURNING
- explicit blockers: GENERATED IDENTITY, DEFERRABLE FK
```

### Verification

```bash
rg -n "Prisma|ActiveRecord|SHOW FULL|TABLE STATUS|IDENTITY|DEFERRABLE" docs crates
```

## Step 2 — Add a bounded compatibility smoke

**Goal:** codify the tier-2 baseline as tests instead of only prose.
**Files:** likely `tools/wire-test.py` and/or one SQL/network integration test.
**Approach:** prefer one or two realistic ORM-flavored flows over a broad fake matrix.

### Candidate smoke

```text
1. Connect + metadata discovery queries used by ActiveRecord / Prisma.
2. Migration-style flow:
   - CREATE TABLE with AUTO_INCREMENT / SERIAL
   - INSERT ... RETURNING
   - SHOW FULL metadata introspection
3. Explicit assertion that identity/deferred-FK features remain documented blockers.
```

### Verification

```bash
cargo test -p axiomdb-network
# and/or
cargo test -p axiomdb-sql --test <compat test>
python3 tools/wire-test.py
```

## Step 3 — Implement only the narrow missing shims

**Goal:** fix the smallest audited gaps required by the smoke.
**Files:** wire intercept / SHOW support / parser only as needed.
**Approach:** keep fixes incremental and compatibility-oriented.

### Implementation outline

- If `SHOW FULL TABLES` is missing, implement that exact variant.
- If `SHOW FULL COLUMNS` is missing, implement that exact variant.
- If `SHOW TABLE STATUS` is missing or incomplete, implement only the fields
  required by the smoke / current ORM discovery path.
- Avoid expanding into identity columns or deferred FKs here.

### Verification

```bash
cargo test -p axiomdb-network
cargo test -p axiomdb-sql
```

## Step 4 — Document incompatibilities and close

**Goal:** make the compatibility contract explicit.
**Files:** phase docs / memory / progreso.

### Documentation points

```text
- What now works for Prisma / ActiveRecord tier 2
- Which probes were added or validated
- GENERATED IDENTITY remains 24.1c
- DEFERRABLE / deferred FK remains 21.16
```

### Final verification

```bash
cargo fmt --check
python3 tools/wire-test.py
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

## Risk register

| Risk | Likelihood | Mitigation |
|------|------------|------------|
| `21.24` grows into major feature work | high | keep identity columns and deferred FK explicitly out of scope |
| ORM expectations are underspecified and we overfit to one query | medium | choose a small but realistic audited baseline and document it |
| Wire shims drift from SQL parser/executor behavior | medium | prefer existing SQL paths when practical; use intercepts only for connect/introspection queries |
| Compatibility claim becomes misleading | medium | document exact blockers and supported tier clearly in closeout docs |

## Estimated effort

Total: medium-high

- Step 1: 30-45 min
- Step 2: 45-90 min
- Step 3: 45-120 min
- Step 4: 30-45 min
