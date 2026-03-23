# Spec: 4.12 — DISTINCT

## What to build (not how)

`SELECT DISTINCT` deduplicates the output rows, returning at most one row for
each unique combination of projected column values.

DISTINCT operates on the **projected output rows** (after SELECT list
evaluation), not on source rows. Two rows are considered identical if every
column value in both rows is equal under the same rules used by GROUP BY key
hashing: `NULL = NULL` (two NULL values are considered equal for deduplication
purposes — unlike equality comparison where `NULL <> NULL`).

---

## Pipeline position

```
scan → join → WHERE → GROUP BY → HAVING → ORDER BY → PROJECT → DISTINCT → LIMIT
```

DISTINCT is applied **after projection** and **before LIMIT**. ORDER BY
operates on pre-projection source rows (as established in Phase 4.10) and
produces the same ordered result regardless of DISTINCT.

**Why after projection:** DISTINCT must compare the values that the caller
actually receives — the projected column expressions. Two source rows may map
to identical projected values even if the source rows differ.

**Why before LIMIT:** `LIMIT 3` on a `SELECT DISTINCT` query means "3 distinct
rows", not "deduplicate after taking 3 rows from the full set".

---

## Row identity for deduplication

Two output rows are identical if their serialized byte representations are
equal. Use `value_to_key_bytes` (already in `executor.rs` for GROUP BY) to
serialize each value in the row, then concatenate all serialized values into a
single `Vec<u8>` key.

**NULL semantics:** `value_to_key_bytes(Null)` = `[0x00]`, so two NULL values
produce identical bytes → they are considered equal → only one row is kept.
This matches SQL DISTINCT semantics (NULLs are considered "the same" for
deduplication, unlike `NULL = NULL` which is UNKNOWN).

---

## DISTINCT + ORDER BY

`SELECT DISTINCT col FROM t ORDER BY col` — ORDER BY applies to source rows
before projection; projection preserves sort order. The result is correctly
distinct and ordered.

**Restriction (not enforced in Phase 4.12):** SQL standard requires that when
DISTINCT is active, ORDER BY may only reference columns present in the SELECT
list. Example:

```sql
-- Valid: `name` is in SELECT list
SELECT DISTINCT name FROM users ORDER BY name;

-- Invalid per standard: `age` not in SELECT list
SELECT DISTINCT name FROM users ORDER BY age;
```

The second query is rejected by most databases. In Phase 4.12, AxiomDB does
NOT enforce this restriction — it applies ORDER BY against source rows before
projection, which works but may produce non-standard results when ordering by
a column not in the SELECT list. Phase 6 (semantic analysis improvements) will
add enforcement.

---

## DISTINCT + GROUP BY

`SELECT DISTINCT dept, COUNT(*) FROM employees GROUP BY dept` — DISTINCT on
GROUP BY output. Since each (dept, COUNT(*)) combination is already unique
(GROUP BY guarantees one row per group), DISTINCT has no effect. Permitted and
works correctly.

---

## DISTINCT + *

`SELECT DISTINCT * FROM t` — expands to all columns, then deduplicates rows
where every column value is equal. Works correctly.

---

## Inputs / Outputs

| Input | Output |
|---|---|
| `stmt.distinct: bool` | rows with duplicates removed |

`QueryResult::Rows` is unchanged — DISTINCT only affects which rows are
present, not the column schema.

---

## Use cases

### 1. Deduplicate a column

```sql
SELECT DISTINCT dept FROM employees;
-- Returns: [('eng',), ('sales',)] — one row per unique department
```

### 2. Deduplicate multiple columns

```sql
SELECT DISTINCT dept, role FROM employees;
-- Returns unique (dept, role) pairs
```

### 3. DISTINCT with WHERE

```sql
SELECT DISTINCT dept FROM employees WHERE salary > 80000;
```

### 4. DISTINCT * on table with duplicates

```sql
SELECT DISTINCT * FROM t;
-- Returns unique full rows
```

### 5. DISTINCT on single row — no effect

```sql
SELECT DISTINCT 1;
-- Returns: [(1,)] — one row, same as without DISTINCT
```

### 6. DISTINCT on empty table

```sql
SELECT DISTINCT dept FROM empty_t;
-- Returns: [] — empty result
```

### 7. DISTINCT with NULL values

```sql
-- t has rows: (NULL), (1), (NULL), (2)
SELECT DISTINCT val FROM t;
-- Returns: [(NULL,), (1,), (2,)] — only one NULL row
```

### 8. DISTINCT + ORDER BY

```sql
SELECT DISTINCT dept FROM employees ORDER BY dept ASC;
-- Returns unique departments in alphabetical order
```

---

## Acceptance criteria

- [ ] `SELECT DISTINCT col` returns only unique values
- [ ] `SELECT DISTINCT col1, col2` deduplicates by the (col1, col2) pair
- [ ] Two rows with `NULL` in the same column → kept as one row (NULLs equal)
- [ ] `SELECT DISTINCT *` deduplicates full rows
- [ ] `SELECT DISTINCT 1` returns one row (scalar without FROM)
- [ ] Empty table → 0 rows
- [ ] DISTINCT + WHERE: filter before dedup
- [ ] DISTINCT + ORDER BY: deduplicated rows in sorted order
- [ ] DISTINCT + LIMIT: dedup first, then limit
- [ ] DISTINCT + GROUP BY: works (may be no-op if groups already unique)
- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace -- -D warnings` passes
- [ ] `cargo fmt --check` passes
- [ ] No `unwrap()` in `src/` outside tests

---

## Out of scope

- Enforcement of "ORDER BY must reference SELECT list columns" with DISTINCT
  (Phase 6)
- `DISTINCT ON (col)` PostgreSQL extension (future)
- Streaming deduplication (Phase 14)

---

## Dependencies

- `axiomdb-sql/src/executor.rs` — only file modified
- `value_to_key_bytes` already present — reused directly
- No new crates, no new dependencies
