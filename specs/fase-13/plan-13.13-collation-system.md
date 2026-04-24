# Plan: 13.13 — Collation system

Phase: 13 — Advanced PostgreSQL
Task: 13.13 Collation system
Spec: specs/fase-13/spec-13.13-collation-system.md
Status: draft

## Summary

This plan closes `13.13` as the first real layered collation system, not as
full ICU parity. The order is: formalize a small registry and alias map, stop
discarding persisted DDL collation metadata, resolve effective collation in the
executor with deterministic precedence, expose it through introspection, and
fix the stale docs that still claim UCA/ICU is already live.

## Dependencies

Must be done first:

- [ ] `spec-13.13-collation-system.md` approved
- [x] `13.12` closed

Blocks:

- [ ] any future ICU / UCA root work should build on this precedence model

## Affected areas

Likely files:

- `crates/axiomdb-sql/src/ast.rs`
- `crates/axiomdb-sql/src/parser/ddl.rs`
- `crates/axiomdb-sql/src/parser/*` for query-time `COLLATE`
- `crates/axiomdb-sql/src/session.rs`
- `crates/axiomdb-sql/src/text_semantics.rs`
- `crates/axiomdb-sql/src/eval/*`
- `crates/axiomdb-sql/src/executor/*`
- `crates/axiomdb-catalog/src/schema_table.rs`
- `crates/axiomdb-catalog/src/writer.rs`
- `crates/axiomdb-catalog/src/reader.rs`
- `tools/wire-test.py`
- docs / docs-site / memory

## Step 1 — Registry and normalization

**Goal:** centralize accepted collation names and alias normalization.

**Deliverables:**

- canonical registry for `binary` and `es`
- alias map for MySQL / PG-facing names
- user-facing validation helper

### Verification

```bash
cargo test -p axiomdb-sql session
```

## Step 2 — Persist DDL metadata

**Goal:** stop discarding meaningful `COLLATE` clauses.

**Deliverables:**

- AST carries table / column / database collation options
- catalog persists normalized names
- CREATE/ALTER/SHOW CREATE round-trip the metadata

### Verification

```bash
cargo test -p axiomdb-sql --test integration_ddl_parser --test integration_executor_ddl
```

## Step 3 — Query-time `COLLATE` and precedence

**Goal:** allow explicit expression overrides and make precedence deterministic.

**Deliverables:**

- parse `expr COLLATE ident`
- analyzer/executor resolve effective collation by scope
- conflicting explicit collations error clearly

### Verification

```bash
cargo test -p axiomdb-sql --test integration_eval --test integration_executor_query
```

## Step 4 — Executor propagation and safety

**Goal:** ensure every text-sensitive operator uses the resolved collation.

**Deliverables:**

- comparisons / `LIKE` / ORDER BY / GROUP BY / DISTINCT use the effective collation
- existing text-index safety gate remains intact under non-binary collation
- EXPLAIN / planner behavior stays honest

### Verification

```bash
cargo test -p axiomdb-sql --test integration_indexes --test integration_index_only
```

## Step 5 — Introspection, smoke, and closeout

**Goal:** make the feature externally visible and document it honestly.

**Deliverables:**

- metadata surfaces (`SHOW CREATE TABLE`, `SHOW FULL COLUMNS`, maybe information_schema)
- wire smoke for table/column/query override
- docs + docs-site aligned with delivered scope

### Verification against spec

- [ ] normalized registry exists
- [ ] metadata persists
- [ ] precedence is deterministic
- [ ] query-time `COLLATE` works
- [ ] introspection reflects stored values
- [ ] stale ICU/UCA claims removed or narrowed
- [ ] wire smoke is green
- [ ] `cargo test -p axiomdb-sql` for touched tests passes
- [ ] `python3 tools/wire-test.py` passes
- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace -- -D warnings` passes

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| doc promises and shipped behavior diverge again | high | treat docs alignment as a first-class acceptance criterion |
| text comparison precedence becomes implicit or inconsistent | high | centralize effective-collation resolution in one helper |
| query-time `COLLATE` widens too far into full type coercion work | medium | restrict 13.13 to text expressions only |
| index correctness regresses under non-binary semantics | medium | preserve current text-index opt-out rule |

## Estimated effort

Total: high

Per step:

- Step 1: 30–60 min
- Step 2: 1–2 h
- Step 3: 1–2 h
- Step 4: 1–2 h
- Step 5: 45–75 min
