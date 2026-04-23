# Spec: 21.16 — DEFERRABLE constraints

Phase: 21 — Advanced SQL
Task: 21.16 DEFERRABLE constraints
Status: implemented

## Context

`docs/progreso.md` still tracks `21.16` as pending:

- `DEFERRABLE`
- `INITIALLY DEFERRED`
- `INITIALLY IMMEDIATE`
- transaction-end verification on `COMMIT`

The repository already has immediate constraint enforcement for:

- foreign keys in `crates/axiomdb-sql/src/fk_enforcement.rs`
- CHECK constraints in insert/update/ODKU/MERGE paths
- exclusion constraints via owned helper unique indexes

What is missing is not generic constraint infrastructure; it is deferred
transaction-time enforcement. The most valuable user-visible need is classic
bulk-load / graph-insert FK ordering, where rows may be temporarily invalid
inside the transaction but valid by `COMMIT`.

Trying to make **all** constraint kinds deferrable in one subphase would be the
wrong cut:

- deferred CHECK would require buffering row images or re-validating every
  touched row across many executor paths
- deferred exclusion would need owned-helper-index conflict postponement or a
  second commit-time conflict detector
- `SET CONSTRAINTS` would add per-transaction mode switching and savepoint
  semantics on top of the base feature

The bounded, useful MVP is therefore: **foreign keys only**.

## Goal

Implement `DEFERRABLE INITIALLY DEFERRED/IMMEDIATE` for foreign key
constraints, with commit-time validation and full transaction rollback on
violation.

## Non-goals

- Deferred CHECK constraints in this subphase.
- Deferred exclusion constraints in this subphase.
- `SET CONSTRAINTS ...` session/transaction commands in this subphase.
- Mid-transaction switching between immediate and deferred modes.
- Deferring `NOT NULL`, `UNIQUE`, or primary-key enforcement.
- Persisted SQL-standard match modes beyond the existing engine behavior.

## SQL surface

Accepted in this subphase:

```sql
CREATE TABLE orders (
  customer_id INT,
  CONSTRAINT fk_customer
    FOREIGN KEY (customer_id) REFERENCES customers(id)
    DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE orders (
  customer_id INT REFERENCES customers(id)
    DEFERRABLE INITIALLY IMMEDIATE
);

ALTER TABLE orders
  ADD CONSTRAINT fk_customer
  FOREIGN KEY (customer_id) REFERENCES customers(id)
  DEFERRABLE;
```

Supported modifiers:

- `DEFERRABLE`
- `DEFERRABLE INITIALLY DEFERRED`
- `DEFERRABLE INITIALLY IMMEDIATE`
- `NOT DEFERRABLE`

Default when omitted:

- `NOT DEFERRABLE INITIALLY IMMEDIATE`

Rejected / out of scope:

```sql
SET CONSTRAINTS ALL DEFERRED;
SET CONSTRAINTS fk_customer IMMEDIATE;
CHECK (x > 0) DEFERRABLE;
EXCLUDE (...) DEFERRABLE;
```

## Public API / AST / catalog

### AST

Foreign-key declarations gain deferrability metadata:

```rust
pub enum ConstraintTiming {
    Immediate,
    Deferred,
}

pub struct ConstraintDeferrability {
    pub deferrable: bool,
    pub initially: ConstraintTiming,
}
```

Both column-level `REFERENCES ...` and table-level `FOREIGN KEY ...` store this
metadata in the AST. The parser normalizes omitted clauses to
`deferrable = false`, `initially = Immediate`.

### Catalog

`FkDef` persists:

```rust
pub struct FkDef {
    // existing fields...
    pub deferrable: bool,
    pub initially_deferred: bool,
}
```

Compatibility rule:

- legacy FK rows without the new trailer decode as
  `deferrable = false`, `initially_deferred = false`

## Semantics

### Immediate vs deferred

- `NOT DEFERRABLE` FKs keep current behavior: every statement validates
  immediately.
- `DEFERRABLE INITIALLY IMMEDIATE` also validates immediately in this MVP,
  because `SET CONSTRAINTS` is out of scope.
- `DEFERRABLE INITIALLY DEFERRED` skips statement-time FK rejection and instead
  queues commit-time validation work.

### Commit-time validation

For every transaction that touched deferred FKs, the engine validates at
`COMMIT`:

1. child-side inserts/updates still reference an existing parent row
2. parent-side deletes/updates did not leave illegal dangling children after
   all in-transaction cascades / repairs

If any deferred FK fails at commit time:

- `COMMIT` returns the corresponding FK violation error
- the full transaction is rolled back
- no partial commit is visible

### Scope of deferred tracking

The engine tracks only rows relevant to deferred foreign keys:

- child rows inserted or updated on tables owning deferred FKs
- parent rows deleted or key-updated on tables referenced by deferred FKs

It does not need a general constraint queue for unrelated statement types.

### Transaction boundaries

- Validation happens only for a real open transaction.
- In autocommit mode, a single statement still commits immediately, so deferred
  FKs behave effectively like immediate checks from the user's perspective.
- On `ROLLBACK`, all queued deferred-FK validation state is discarded.
- On successful `COMMIT`, queued deferred-FK state is cleared.

### Savepoints

Deferred validation state must follow savepoint rollback:

- rows touched after a savepoint disappear from the deferred queue after
  `ROLLBACK TO SAVEPOINT`
- releasing a savepoint does not force validation

The simplest acceptable implementation is to keep deferred-FK tracking inside
`ConnectionTxn` so the existing savepoint machinery can snapshot/truncate it.

## Implementation model

### Parser / AST

- Extend FK grammar to consume optional
  `DEFERRABLE | NOT DEFERRABLE [INITIALLY DEFERRED|IMMEDIATE]`
- store metadata on both column-level and table-level FK declarations

### Catalog

- persist deferrability flags on `FkDef`
- decode old rows compatibly

### Executor / enforcement

- immediate paths continue to call existing FK enforcement helpers
- deferred paths record enough information for later validation instead of
  erroring immediately

Suggested queue shapes:

- child check queue: `(fk_id, child_table_id, row_values)`
- parent check queue: `(fk_id, parent_table_id, old_parent_row, operation_kind)`

The exact in-memory shape is implementation-defined; the contract is commit-time
revalidation against the transaction's final visible state.

### Commit integration

Before WAL commit is finalized:

1. run deferred FK validation using the current connection snapshot / roots
2. if validation fails, roll back the transaction instead of committing
3. otherwise proceed with normal commit

## Edge cases

- [ ] Omitted clause defaults to `NOT DEFERRABLE INITIALLY IMMEDIATE`.
- [ ] `NOT DEFERRABLE INITIALLY DEFERRED` is rejected as invalid syntax/semantics.
- [ ] Column-level and table-level FK declarations both preserve deferrability.
- [ ] Legacy catalog rows decode as non-deferrable.
- [ ] `DEFERRABLE INITIALLY IMMEDIATE` behaves like current immediate checks.
- [ ] `DEFERRABLE INITIALLY DEFERRED` allows child-before-parent insert within
      one explicit transaction when the parent is inserted later.
- [ ] Deferred FK violation is raised by `COMMIT`, not by the earlier DML.
- [ ] `ROLLBACK` discards deferred validation work.
- [ ] `ROLLBACK TO SAVEPOINT` discards only the deferred validation work added
      after that savepoint.
- [ ] Parent delete/update violations on deferred FKs are also detected at
      commit time.
- [ ] Existing non-deferrable FK tests remain green.

## Acceptance criteria

1. [x] Parser accepts FK `DEFERRABLE`, `DEFERRABLE INITIALLY DEFERRED`,
       `DEFERRABLE INITIALLY IMMEDIATE`, and `NOT DEFERRABLE`.
2. [x] Catalog persists FK deferrability and reads legacy rows compatibly.
3. [x] `DEFERRABLE INITIALLY DEFERRED` allows out-of-order parent/child writes
       inside one explicit transaction when the final committed state is valid.
4. [x] Invalid deferred FK state causes `COMMIT` to fail and roll back.
5. [x] Savepoint rollback truncates deferred FK tracking correctly.
6. [x] Existing immediate FK semantics remain unchanged for non-deferrable FKs.
7. [x] `python3 tools/wire-test.py` gains at least one `21.16` smoke.

## Out-of-scope follow-ups

- `SET CONSTRAINTS ALL/constraint_name DEFERRED|IMMEDIATE`
- deferred CHECK / exclusion / unique constraints
- richer transaction-mode introspection
- deferrable constraints beyond foreign keys

## Performance budget

- Immediate non-deferrable FK paths must not regress materially.
- Deferred mode may add per-row queue bookkeeping, but commit-time validation
  should scale with the number of touched deferred-FK rows, not full-table scans
  by default.
