# Spec: 11.18a — JSONB PostgreSQL operator parity (phase A)

## What to build (not how)

PostgreSQL-compatible JSONB operator surface, scoped to the five
operators that do **not** require a native `TEXT[]` type. Extends
Phase 11.16 (binary JSONB + JSONPath + `->`) and Phase 11.17 (GIN
index for `@>`) with:

| Operator | Signature | Meaning |
|----------|-----------|---------|
| `?`  | `jsonb ? text → bool` | Object top-level key exists, or text appears as a string element of a JSONB array. |
| `<@` | `jsonb <@ jsonb → bool` | Left side is structurally contained in right (reverse of `@>`). |
| `\|\|` | `jsonb \|\| jsonb → jsonb` | Concatenation: object+object = shallow merge (RHS wins), array+array = append, mixed = array wrap. |
| `-`  | `jsonb - text → jsonb` | Delete a top-level object key. When LHS is an array, delete every string element equal to RHS. |
| `-`  | `jsonb - int → jsonb` | Delete the element at the given array index; negative indices count from the end. Error on object LHS. |

Plus:

- **Function aliases** for every operator (`JSONB_EXISTS`,
  `JSONB_CONTAINED`, `JSONB_CONCAT`, `JSONB_DELETE_KEY`,
  `JSONB_DELETE_INDEX`) so cross-engine-portable SQL (MySQL, MariaDB,
  DuckDB users) can call the feature without PG-specific operators.
- **GIN planner integration** for `?` via the existing Phase 11.17
  term layout — `WHERE col ? 'key'` plans as `GinScan` with
  `recheck = true` (PG parity: `gin_consistent_jsonb` strategy 9).
- Explicit **scope boundary**: every operator with a `text[]`
  right-hand side (`?|`, `?&`, `-(text[])`, `#-`, `#>`, `#>>`) is
  deferred to `spec-11.18b-jsonb-array-operators.md` because a
  robust implementation requires a SQL `TEXT[]` type that does not
  yet exist in AxiomDB. 11.18b will either introduce `TEXT[]` or
  accept JSONB array RHS with documented PG divergence.

Behavior matches PostgreSQL 16 `src/backend/utils/adt/jsonb_op.c` and
`jsonfuncs.c` exactly for the listed operators — same NULL handling,
same error raises, same deep-structural semantics.

## Inputs / Outputs

### SQL syntax accepted

```sql
SELECT '{"a":1,"b":2}'::jsonb ? 'a';                    -- true
SELECT '["x","y"]'::jsonb ? 'x';                        -- true (array string)
SELECT '{"a":1}'::jsonb <@ '{"a":1,"b":2}'::jsonb;      -- true
SELECT '{"a":1}'::jsonb || '{"b":2}'::jsonb;            -- {"a":1,"b":2}
SELECT '[1,2]'::jsonb || '[3]'::jsonb;                  -- [1,2,3]
SELECT '{"a":1,"b":2}'::jsonb - 'a';                    -- {"b":2}
SELECT '["x","y","z"]'::jsonb - 'y';                    -- ["x","z"]
SELECT '[1,2,3]'::jsonb - 0;                            -- [2,3]
SELECT '[1,2,3]'::jsonb - (-1);                         -- [1,2]
```

### Function-style equivalents

```sql
JSONB_EXISTS(doc, 'a')             -- identical to doc ? 'a'
JSONB_CONTAINED(a, b)              -- identical to a <@ b
JSONB_CONCAT(a, b)                 -- identical to a || b
JSONB_DELETE_KEY(doc, 'a')         -- identical to doc - 'a'
JSONB_DELETE_INDEX(doc, 0)         -- identical to doc - 0
```

### Outputs

- Boolean operators return `Value::Bool(_)` or `Value::Null` when an
  operand is NULL (propagates like every other binary op).
- JSONB operators return `Value::Jsonb(_)` or `Value::Null`.

## Use cases

1. **Feature flag lookup** — `WHERE flags ? 'beta_enabled'` with a GIN
   index on `flags` makes it O(log N) instead of full scan.
2. **Config inheritance check** — `WHERE user_config <@ default_config`
   finds users that haven't overridden anything.
3. **Merge patch** — `UPDATE t SET doc = doc || '{"version": 2}'`
   applies a shallow update without a custom function.
4. **Tag removal** — `UPDATE posts SET tags = tags - 'draft'` removes
   a tag from an array.
5. **Trim oldest event** — `UPDATE timeline SET events = events - 0`
   pops the head of a JSON array.
6. **Cross-engine portable call** — application code can write
   `WHERE JSONB_EXISTS(doc, 'key')` and the same SQL executes on
   AxiomDB today and on PG as a cheap wrapper tomorrow.

## Acceptance criteria

- [ ] **`?` operator** — object keys + array string elements; false for
      non-object/non-array; NULL propagates.
- [ ] **`<@` operator** — uses the same deep-containment walk as `@>`
      but with arguments reversed; returns false on type mismatch (not
      error), matching `jsonb_op.c:130-146`.
- [ ] **`||` operator**:
  - object + object → shallow merge, RHS keys override on collision;
  - array + array → append RHS elements to LHS in order;
  - object + array → `[obj, ...arr]`;
  - array + object → `[...arr, obj]`;
  - scalar + anything → wrap scalar as 1-element array, then apply
    array rules;
  - NULL propagates.
- [ ] **`-(text)`**:
  - object LHS → drop every top-level pair whose key equals RHS;
  - array LHS → drop every string element equal to RHS;
  - scalar LHS → `DbError::InvalidValue` (PG: `ERROR: cannot delete
    from scalar`);
  - no-op when key/element not present.
- [ ] **`-(int)`**:
  - array LHS → drop element at index (negative counts from end);
  - out-of-range index → no-op (PG behavior, not error);
  - object LHS → `DbError::InvalidValue` (PG: `ERROR: cannot delete
    from object using integer index`);
  - scalar LHS → same error.
- [ ] **Function aliases**: identical semantics to the operators;
      listed in `eval/functions/mod.rs`.
- [ ] **GIN planner integration for `?`**:
  - `WHERE col ? 'key'` plans as `GinScan` when a `jsonb_ops` GIN
    index exists on `col`;
  - falls back to full scan when no index;
  - `recheck = true` so the executor re-verifies structurally (dead
    rows + false positives filtered);
  - no crash when the index is empty or the table is empty.
- [ ] **NULL in either operand**: the whole expression is NULL (three-
      valued logic, matches PG and our existing binary ops).
- [ ] **Non-JSONB operand**: automatic `CAST(... AS JSONB)` fails with
      `InvalidCoercion` — same as other JSONB operators.
- [ ] **Wire protocol**: results serialize as JSON text over the MySQL
      wire (consistent with existing JSONB path).
- [ ] **Integration tests** in `crates/axiomdb-sql/tests/integration
      _jsonb_operators.rs` cover every acceptance bullet above plus
      PG-regression-parity inputs (`src/test/regress/sql/jsonb.sql`
      lines 300-333 for `?`, 1135-1172 for `||`, 1174-1197 for `-`,
      245-250 for `<@`).
- [ ] `cargo test --workspace` clean.
- [ ] `cargo clippy --workspace -- -D warnings` clean.
- [ ] `cargo fmt --check` clean.

## Out of scope

- **`?|`, `?&`, `-(text[])`, `#-`, `#>`, `#>>`** — six operators
  requiring a `TEXT[]` right-hand side. Deferred to
  `spec-11.18b-jsonb-array-operators.md`. MVP 11.18b will choose
  between introducing native `TEXT[]` or accepting JSONB array as RHS
  with documented PG divergence.
- **`jsonb_path_ops` GIN opclass** (`@?`, `@@` operators) — deferred
  to Phase 11.21.
- **`jsonb_set`, `jsonb_insert`** etc. — Phase 11.22.
- **Three-valued truth on `?`-with-NULL-RHS**: PG raises; we match by
  returning NULL as other binary ops.
- **Native `TEXT[]` SQL type**: separate feature, not a blocker for
  this phase.

## Dependencies

- Phase 11.16 binary JSONB runtime: `Value::Jsonb(Arc<Vec<u8>>)`,
  `JsonbRef`, existing `->` / `JSON_CONTAINS` / `JSON_OVERLAPS`
  implementations in `crates/axiomdb-types/src/jsonb.rs` and
  `crates/axiomdb-sql/src/eval/functions/jsonb.rs`.
- Phase 11.17 GIN layout: `gin_extract_jsonb` equivalent + term
  posting lists + executor `GinScan` plan node.
- Existing `BinaryOp` enum in `crates/axiomdb-sql/src/expr.rs`.
- Parser precedence table in `crates/axiomdb-sql/src/parser/expr.rs`.
- Lexer token table in `crates/axiomdb-sql/src/lexer.rs` — `?` and
  `<@` need new tokens; `||` is already a token for string concat
  (dispatch by operand type at eval time); `-` is already a token.

## Effort for next step

- **Plan: medium** — one new lexer token (`?`, `<@`), three new
  `BinaryOp` variants, two operator dispatches on `||` and `-` at
  eval time, GIN planner extension is tight enough to isolate.
