# Plan: 11.18a — JSONB PostgreSQL operator parity (phase A)

## Files to create/modify

**Modify:**

- `crates/axiomdb-sql/src/lexer.rs`
  - Add `Token::JsonContainedBy` for `<@` (must appear before `@>`
    in the token table so the LR scanner prefers the longer match
    when the input starts with `<`).
  - `?` already tokenizes as `Token::Question` — it will need a
    context-sensitive reinterpretation in the expression parser
    (currently reserved only for prepared-statement placeholders).

- `crates/axiomdb-sql/src/expr.rs`
  - Add `BinaryOp::JsonExists` (text key/array-string on RHS),
    `BinaryOp::JsonContainedBy` (`<@`), `BinaryOp::JsonConcat`
    (`||` when operands are JSONB), `BinaryOp::JsonDeleteKey`
    (`-` jsonb-text), `BinaryOp::JsonDeleteIdx` (`-` jsonb-int).
  - Document that `||` and `-` are routed by the *parser* when
    one operand is a JSONB literal / CAST / column; runtime
    dispatch in `eval_binary` handles mixed cases by coercing.

- `crates/axiomdb-sql/src/parser/expr.rs`
  - Precedence: `?`, `<@`, `@>` share the same band as `->`
    (custom JSONB operators, lower than comparison, higher than
    `AND`/`OR`). `||` stays where it is (Phase 4 string concat) —
    we reuse the token and pick the right `BinaryOp` variant at
    eval time based on operand type. `-` keeps its arithmetic
    precedence; same type-driven dispatch.
  - Add `Token::Question` in expression contexts where it follows
    a JSONB expression (otherwise it stays a prepared-statement
    placeholder). Disambiguation: if the next non-whitespace
    token is a string/identifier/LParen-expr, it's the JSONB
    exists operator; otherwise it's a placeholder. Detail: the
    current parser tracks `in_stmt_placeholder_context` — we
    hoist a `prev_expr_is_jsonb_candidate` hint from the infix
    loop to decide.
  - Alternative simpler path (chosen): always treat `?` as the
    JSONB-exists infix operator when a `left` JSONB-typed Expr
    has been parsed (the same rule we use for `->` and `@>`).
    Prepared-statement `?` as a standalone atom is untouched
    because it parses in `parse_atom` before we reach infix.

- `crates/axiomdb-sql/src/eval/ops.rs`
  - Extend `eval_binary`:
    - `BinaryOp::JsonExists` → new helper
      `jsonb_exists(doc: &Value, key: &Value) -> Result<Value>`.
    - `BinaryOp::JsonContainedBy` → reuse
      `jsonb_contains(rhs, lhs)` (argument swap).
    - `BinaryOp::JsonConcat` → new helper
      `jsonb_concat(a: &Value, b: &Value) -> Result<Value>`.
    - `BinaryOp::JsonDeleteKey` / `JsonDeleteIdx` → new helper
      `jsonb_delete(doc, key_or_idx) -> Result<Value>` with
      dispatch on the RHS type.
  - For `||` and `-`: when either operand is `Value::Jsonb(_)`,
    route to the JSONB helper; otherwise keep the existing
    string-concat / arithmetic behavior.

- `crates/axiomdb-types/src/jsonb.rs` (or a new
  `crates/axiomdb-types/src/jsonb_ops.rs` sibling)
  - Add:
    - `fn jsonb_exists(doc: &[u8], key: &str) -> Result<bool>` —
      object key OR string array element.
    - `fn jsonb_contained(inner: &[u8], outer: &[u8]) -> Result<bool>`
      — delegates to existing deep-contains in reverse.
    - `fn jsonb_concat(a: &[u8], b: &[u8]) -> Result<Vec<u8>>` —
      five merge rules (obj/obj, arr/arr, obj/arr, arr/obj,
      scalar wrap).
    - `fn jsonb_delete_key(doc: &[u8], key: &str) -> Result<Vec<u8>>`
      — object drop + array string-element drop.
    - `fn jsonb_delete_idx(doc: &[u8], idx: i64) -> Result<Vec<u8>>`
      — array only; negative wraps; out-of-range no-op.
  - All five build on the existing `JsonbRef` iterator + JSONB
    binary encoder from Phase 11.16.

- `crates/axiomdb-sql/src/eval/functions/jsonb.rs`
  - Register function aliases so portable SQL works:
    - `JSONB_EXISTS(doc, key)`
    - `JSONB_CONTAINED(a, b)`
    - `JSONB_CONCAT(a, b)`
    - `JSONB_DELETE_KEY(doc, key)`
    - `JSONB_DELETE_INDEX(doc, idx)`
  - Each alias re-uses the same helper function as the operator.

- `crates/axiomdb-sql/src/planner.rs` (or whichever file hosts the
  GIN planner entry; grep for `GinScan` + `@>` precedent)
  - Extend the "is this predicate GIN-indexable?" predicate so that
    `Expr::BinaryOp { op: JsonExists, left: Column(col), right: Literal(Text) }`
    (with a matching GIN index on `col`) chooses `GinScan` with
    `recheck = true`.
  - Term extraction reuses the 11.17 key-extraction routine — the
    "key" term in the current term layout is exactly what `?`
    probes for.

- `crates/axiomdb-sql/tests/integration_jsonb_operators.rs` (**new**)
  - Coverage matrix (see **Tests** below).

**No change**:

- `axiomdb-wal`, `axiomdb-storage`, `axiomdb-index` — the GIN
  term layout from 11.17 is reused as-is.

## Algorithm / Data structure

### `?` operator

```text
jsonb_exists(doc, key):
    let root = JsonbRef::from_bytes(doc)
    match root.kind():
        Object → iterate keys; return true if any key == `key`
        Array  → iterate elements; return true if any element is
                 a JSONB string equal to `key`
        other  → false   # PG matches: scalar ? _ = false
```

### `<@` operator

```text
jsonb_contained(inner, outer):
    return jsonb_contains(outer, inner)
```

Existing `jsonb_contains` already implements deep structural
containment per Phase 11.16; no new logic.

### `||` operator

```text
jsonb_concat(a, b):
    let ka = kind(a); let kb = kind(b)
    match (ka, kb):
        (Object, Object) → shallow merge into a new object; RHS
                           keys override LHS on collision
        (Array, Array)   → append arr_a, arr_b
        (Object, Array)  → [obj_a, ...arr_b]
        (Array, Object)  → [...arr_a, obj_b]
        (_, _)           → wrap the scalar sides as 1-element arrays
                           and recurse (PG behavior)
```

Uses the existing JSONB binary writer (`JsonbBuilder`) from
Phase 11.16 to emit the result in one pass.

### `-` operator (jsonb minus text)

```text
jsonb_delete_key(doc, key):
    match kind(doc):
        Object → emit a new object skipping every (k, v) where k == key
        Array  → emit a new array skipping every string element == key
        Scalar → Err(DbError::InvalidValue {
                    reason: "cannot delete from scalar JSONB" })
```

### `-` operator (jsonb minus int)

```text
jsonb_delete_idx(doc, idx):
    match kind(doc):
        Array →
            n = len(arr)
            real_idx = if idx < 0 { n + idx } else { idx }
            if real_idx < 0 or real_idx >= n: return doc unchanged
            emit new array skipping element at real_idx
        Object → Err(DbError::InvalidValue {
                    reason: "cannot delete from object using integer index" })
        Scalar → Err(DbError::InvalidValue {
                    reason: "cannot delete from scalar JSONB" })
```

### Parser dispatch for type-overloaded operators

At parse time we don't know operand types. The AST holds
`BinaryOp::Concat` (string) and `BinaryOp::Sub` (minus) for the
generic infix tokens `||` and `-`. At eval time in `eval_binary`:

```rust
BinaryOp::Concat => match (left_val, right_val) {
    (Value::Jsonb(a), Value::Jsonb(b)) => jsonb_concat(&a, &b),
    (a, b) => text_concat(a, b),          // existing behavior
},
BinaryOp::Sub => match (left_val, right_val) {
    (Value::Jsonb(doc), Value::Text(key))  => jsonb_delete_key(&doc, &key),
    (Value::Jsonb(doc), Value::Int(i))     => jsonb_delete_idx(&doc, i as i64),
    (Value::Jsonb(doc), Value::BigInt(i))  => jsonb_delete_idx(&doc, i),
    (a, b) => arithmetic_sub(a, b),       // existing behavior
},
```

This keeps the AST small: no new parser work to decide between
string-concat and jsonb-concat — the type system does.

### GIN planner integration for `?`

```text
is_jsonb_exists_sargable(expr):
    let BinaryOp { op: JsonExists, left, right } = expr
    let Column { col_idx } = left
    let Literal(Value::Text(key)) = right
    find jsonb_ops GIN index on (col_idx); return it with terms = [key]

on match → emit GinScan {
    index_id, query_terms: [key],
    recheck: true,                    // MUST always recheck
    recheck_expr: original BinaryOp,
}
```

When GinScan is chosen, the executor post-filters by re-evaluating
the `?` expression against each candidate row — same pattern already
used for `@>` in Phase 11.17.

## Implementation phases

1. **Lexer + expr BinaryOp variants** (~30 LOC):
   `Token::JsonContainedBy`, three new `BinaryOp` values.
2. **JSONB helpers in axiomdb-types** (~220 LOC):
   `jsonb_exists`, `jsonb_contained`, `jsonb_concat`,
   `jsonb_delete_key`, `jsonb_delete_idx`. Unit tests inside the
   crate for each edge case.
3. **eval_binary dispatch** (~40 LOC):
   new arms + overload of `||` and `-` on JSONB operands.
4. **Parser wiring** (~40 LOC):
   `?` and `<@` in the infix precedence table; `?` as JSONB
   exists when the left side is JSONB-shaped.
5. **Function aliases** (~50 LOC):
   register in `eval/functions/jsonb.rs`.
6. **GIN planner integration** (~60 LOC):
   detect `?` predicates + reuse 11.17 term extraction + set
   `recheck = true`.
7. **Integration tests** (~350 LOC):
   coverage matrix.
8. **Close**: progreso.md, clippy/fmt/workspace clean, commit.

## Tests to write

In `tests/integration_jsonb_operators.rs` (~18 tests):

1. `exists_on_object_key` — `'{"a":1}'::jsonb ? 'a'` → true.
2. `exists_on_array_string_element` — `'["x","y"]'::jsonb ? 'x'` → true.
3. `exists_false_on_non_string_array_element` —
   `'[1,2,3]'::jsonb ? '1'` → false (PG behavior).
4. `exists_on_scalar_is_false` — `'42'::jsonb ? 'a'` → false.
5. `exists_null_propagates` — NULL in either operand → NULL.
6. `contained_by_deep_structural` —
   `'{"a":1}'::jsonb <@ '{"a":1,"b":2}'::jsonb` → true.
7. `contained_by_type_mismatch_is_false` — object `<@` array → false.
8. `concat_object_object_rhs_wins` —
   `'{"a":1}'::jsonb || '{"a":9,"b":2}'::jsonb` → `{"a":9,"b":2}`.
9. `concat_array_array_appends`.
10. `concat_object_array_wraps` — `{"a":1}` || `[2]` → `[{"a":1},2]`.
11. `concat_scalar_with_array_wraps`.
12. `delete_key_from_object`.
13. `delete_key_from_array_drops_matching_strings`.
14. `delete_key_on_scalar_errors` — asserts `InvalidValue`.
15. `delete_idx_negative_counts_from_end`.
16. `delete_idx_out_of_range_is_noop`.
17. `delete_idx_on_object_errors`.
18. `function_aliases_match_operators` — asserts
    `JSONB_EXISTS/CONTAINED/CONCAT/DELETE_KEY/INDEX` produce the
    same rows as the operator forms.
19. `gin_plan_for_exists_uses_ginscan` —
    `CREATE INDEX ... USING GIN` + `EXPLAIN WHERE col ? 'key'` plan
    contains `GinScan`; `SELECT` returns the correct rows.
20. `gin_empty_index_empty_table` — no crash, returns 0 rows.

## Anti-patterns to avoid

- **Do not** introduce a distinct `BinaryOp::StrConcat` vs
  `BinaryOp::JsonConcat` on the AST. The type-directed dispatch
  at eval time keeps the AST stable and mirrors how PG handles
  polymorphic operators.
- **Do not** bypass the existing `JsonbBuilder` when emitting merged
  / deleted JSONB. The builder owns the key-sort invariant and
  JEntry stride math — reimplementing either is a bug farm.
- **Do not** allow `?` on the LHS of a prepared-statement argument
  list (`?` in positional-parameter position stays a placeholder).
  The parser must only treat `?` as the infix JSONB operator after
  a completed left expression — not as a prefix atom.
- **Do not** forget `recheck = true` on the GinScan plan. Without
  it, dead rows or term false-positives leak into the result set.
- **Do not** wire `<@` as an alias of `@>` with swapped arguments at
  the AST level. Emit it as its own `BinaryOp` so EXPLAIN shows what
  the user actually wrote; the eval delegates to the shared helper.

## Risks

1. **`?` ambiguity with placeholder**. Mitigation chosen: treat `?`
   as the JSONB-exists operator *only* when appearing in infix
   position after a completed expression; prefix `?` keeps
   placeholder semantics. Cross-check with every existing wire
   test (`integration_protocol.rs` binary placeholders).

2. **Lexer ordering for `<@`**. Must appear before `@` in the logos
   token table or the lexer will split `<@` into `<` + `@`.
   Mitigation: place the `#[token("<@")]` entry right next to the
   existing `@>` one; add a lexer unit test asserting the split.

3. **Type-overloaded `||` and `-`**: existing tests around
   arithmetic `-` and string `||` must keep passing. Mitigation:
   route through JSONB helpers ONLY when both operands are
   `Value::Jsonb(_)` (or for `-`, LHS is Jsonb + RHS is Text/Int).
   Run the full `cargo test` after each step.

4. **GIN re-check correctness**: Phase 11.17 already sets recheck
   for `@>`; extending to `?` must not drop recheck when indexes
   have duplicated or stale entries. Mitigation: add an integration
   test that asserts correct results on a table with concurrent
   inserts / deletes after index creation.

5. **Merge collision semantics for `||`**: PG uses RHS-wins on key
   conflict. If we accidentally adopt LHS-wins (a common bug when
   merging two `JsonbBuilder`s), every UPDATE-patch pattern silently
   produces wrong results. Mitigation: test #8 asserts the exact PG
   behavior.

6. **Session & scoped correctness**: text[]-needing operators must
   not accidentally parse — calls like `WHERE col ?| ARRAY[...]`
   should yield a parse error, not a silent wrong answer.
   Mitigation: an explicit test asserting `?|`, `?&`, `#>`, `#>>`,
   `#-` raise parse errors today (pointing to spec-11.18b).
