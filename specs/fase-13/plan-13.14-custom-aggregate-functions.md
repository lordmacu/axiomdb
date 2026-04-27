# Plan: 13.14 — Custom aggregate functions

Phase: 13 — Advanced PostgreSQL
Task: 13.14 Custom aggregate functions
Spec: specs/fase-13/spec-13.14-custom-aggregate-functions.md
Status: draft

## Summary

This plan closes `13.14` as real `CREATE AGGREGATE` support without waiting for
generic `CREATE FUNCTION`. The sequence is: add DDL syntax and catalog
metadata, define a bounded runtime registry for transition/final routines,
teach analyzer/executor to resolve custom aggregate signatures, validate one
useful end-to-end aggregate such as `median`, and then close the docs honestly.

## Dependencies

Must be done first:

- [ ] `spec-13.14-custom-aggregate-functions.md` approved
- [x] `13.13` closed

Blocks:

- [ ] future generic UDF work in `16.1` should not be assumed by this subphase
- [ ] future ordered-set / window aggregate work should build on the same
      catalog identity model if reused

## Affected areas

Likely files:

- `crates/axiomdb-sql/src/ast.rs`
- `crates/axiomdb-sql/src/parser/*`
- `crates/axiomdb-sql/src/analyzer_stmt.rs`
- `crates/axiomdb-sql/src/executor/agg_descriptor.rs`
- `crates/axiomdb-sql/src/executor/agg_accum.rs`
- `crates/axiomdb-sql/src/executor/*`
- `crates/axiomdb-catalog/src/*`
- `crates/axiomdb-network/src/mysql/handler.rs` if metadata/read-only routing is needed
- `tools/wire-test.py`
- docs / docs-site / memory

## Step 1 — DDL and catalog identity

**Goal:** make custom aggregates first-class catalog objects.

**Deliverables:**

- AST for `CREATE AGGREGATE` and `DROP AGGREGATE`
- parser support for signature + option list (`SFUNC`, `STYPE`, `FINALFUNC`)
- catalog row format for aggregate definitions
- duplicate-signature validation

### Verification

```bash
cargo test -p axiomdb-sql --test integration_ddl_parser
```

## Step 2 — Runtime registry

**Goal:** define the narrow execution boundary for `13.14`.

**Deliverables:**

- internal registry of allowed transition/final routines
- state-kind validation for supported `STYPE` names
- stable error mapping for unknown helper names

### Verification

```bash
cargo test -p axiomdb-sql aggregate
```

## Step 3 — Analyzer and aggregate resolution

**Goal:** stop relying on hardcoded built-in name lists for every aggregate.

**Deliverables:**

- analyzer resolves aggregate calls through built-in + custom lookup
- aggregate signature identity includes argument types, not only lowercase name
- unsupported contexts still fail clearly

### Verification

```bash
cargo test -p axiomdb-sql --test integration_executor_query
```

## Step 4 — Executor accumulation path

**Goal:** execute custom aggregates through the same group pipeline.

**Deliverables:**

- runtime descriptor for custom aggregates
- accumulator variant or trait-backed registry dispatch
- finalize path honoring optional `FINALFUNC`
- one useful shipped custom aggregate, preferably `median`

### Verification

```bash
cargo test -p axiomdb-sql --test integration_aggregates
```

## Step 5 — Introspection, wire smoke, and closeout

**Goal:** make the feature externally testable and document its true boundary.

**Deliverables:**

- `DROP AGGREGATE` regression coverage
- optional `SHOW CREATE AGGREGATE` or equivalent if chosen
- wire smoke for create/use/drop
- docs and docs-site aligned with the bounded registry model

### Verification against spec

- [ ] DDL is parsed and persisted
- [ ] registry validation is deterministic
- [ ] analyzer resolves custom signatures
- [ ] executor runs at least one real custom aggregate
- [ ] built-in aggregates still pass existing coverage
- [ ] create/use/drop regressions exist
- [ ] wire smoke is green
- [ ] docs no longer imply generic UDF aggregate bodies already exist
- [ ] `cargo test -p axiomdb-sql` for touched tests passes
- [ ] `python3 tools/wire-test.py` passes
- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace -- -D warnings` passes

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| scope expands into full `CREATE FUNCTION` | high | keep the registry boundary explicit in parser, catalog, and docs |
| built-in aggregate fast paths regress | high | preserve built-ins as direct descriptors or benchmark after refactor |
| catalog identity ignores argument types and causes collisions | high | key definitions by name + full signature |
| state handling becomes too generic too early | medium | support only the minimal registry-backed state kinds needed for shipped aggregates |
| metadata route diverges between SQL executor and MySQL read-only path | medium | add wire smoke that exercises create/use/drop over the protocol |

## Estimated effort

Total: high

Per step:

- Step 1: 45–90 min
- Step 2: 1–2 h
- Step 3: 1–2 h
- Step 4: 1.5–3 h
- Step 5: 45–90 min
