# Spec: faster INSERT parser (VALUES literal fast-path)

Phase: perf-sqlite-gap — write parity with SQLite (inserts)
Task: reduce AST-build cost on the `INSERT ... VALUES` parse path
Status: approved

## Context

Engine-to-engine, AxiomDB reads are at parity-or-faster with SQLite; **writes are
the open frontier**. On the *fair* `insert_batch` benchmark (both engines run
`BEGIN; N parsed INSERTs; COMMIT` at `synchronous=NORMAL`) AxiomDB is ~5.3× slower.
Per-row diagnosis (`axiomdb_bench --diagnose-insert` / `--diagnose-parse`,
macOS, 10K rows) splits the ~7.1µs/row as: **parse 3.3µs (45%)**, analyze 0.67µs
(9%), execute 3.2µs (45%), snapshot 0.04µs. Within parse, **AST construction is
~2.6µs (76%)** and lexing is ~0.85µs (24%). SQLite parses the same INSERT in
~0.5µs (table-driven, arena-allocated).

Root cause of the AST-build cost: every value in `VALUES (...)` is parsed by the
full precedence-climbing `parse_expr` (`parse_or → parse_xor → parse_and →
parse_not → … → parse_unary → parse_postfix → parse_atom`, ~12 levels), and
`parse_atom` clones the whole current token (`p.peek().clone()`) per value. For a
6-column INSERT that is 6 deep descents + 6 token clones — almost all wasted,
because the values are bare literals.

## Goal

Parse `INSERT ... VALUES (lit, lit, …)` rows with a direct literal fast-path that
skips the precedence ladder, producing a **byte-identical** AST, cutting AST-build
materially.

## Non-goals

- Not a statement/parse cache — the user chose the no-cache path (Attack 22 and the
  DML-cache idea are explicitly out; the existing `run_cached` is post-parse and
  only saves the ~0.67µs analyze).
- Not the executor trims — `prepare_row` codec, the per-row `column_data_types`
  alloc, `encode_row`/PK tightening are a **separate subphase** (own spec).
- Not changing the AST to borrow `&'src str` / arena allocation — invasive, deferred.
- Not making the parser consume (own) the token vector so `StringLit` `String`s can
  be moved instead of cloned — possible future win, deferred.
- Not touching SELECT/DDL/UPDATE/DELETE parse paths.

## Behavior

### Public API

No public API change. Internal helper in `crates/axiomdb-sql/src/parser/dml.rs`:

```rust
/// Parse one VALUES element. Fast-path for a bare literal immediately followed
/// by `,` or `)`; otherwise the full expression parser. The guard guarantees
/// the result is identical to `parse_expr` for the fast-path inputs.
fn parse_value_expr(p: &mut Parser) -> Result<Expr, DbError>;
```

Applied at the two `INSERT ... VALUES` element sites in `parse_insert_body`
(the single-row `vec![parse_expr(p)?]` + `row.push(parse_expr(p)?)` loop, and the
`SET col=val` list is **not** in scope — that uses `parse_expr` for `val`).

### Semantics

`parse_value_expr` fires the fast-path **iff both**:
1. `p.peek()` is one of: `Integer(_)`, `Float(_)`, `HexLit(_)`, `StringLit(_)`,
   `True`, `False`, `Null`; **and**
2. `p.peek_at(1)` is `Token::Comma` or `Token::RParen`.

On fast-path: consume the token and return the literal, using the **exact** same
conversion as `parse_atom` (parser/expr.rs:711–742):

| Token | Expr |
|---|---|
| `Integer(n)` | `Literal(Int(n as i32))` if `i32::MIN..=i32::MAX`, else `Literal(BigInt(n))` |
| `Float(f)` | `Literal(Real(f))` |
| `HexLit(n)` | `Literal(BigInt(n))` |
| `StringLit(s)` | `Literal(Text(s))` (clone the `String` — tokens are borrowed) |
| `True` / `False` | `Literal(Bool(true/false))` |
| `Null` | `Literal(Null)` |

Otherwise: fall back to `parse_expr(p)` unchanged.

- Precondition: parser positioned at the first token of a VALUES element.
- Postcondition: returns the element `Expr` and advances exactly as `parse_expr`
  would; parser position identical on both paths.
- Invariant: **for every input, `parse_value_expr(p)` yields the same `Expr` and
  leaves `p` at the same position as `parse_expr(p)`** (fast-path is a strict,
  behavior-preserving subset).

### Why the AST is byte-identical (correctness lynchpin)

With `peek_at(1) ∈ {Comma, RParen}`, no binary operator, postfix (`[…]`),
PG cast (`::`), `AT TIME ZONE`, or unary follows the literal. So the slow path
descends to `parse_atom` (same `Literal`), then unwinds every precedence level
finding `Comma`/`RParen` (no operator) and every postfix check failing — collapsing
to exactly `Expr::Literal(v)`. Fast and slow paths therefore agree.

Param `?` (`Token::Question`) mutates `p.param_count`; it is **excluded** from the
fast-path set, so counter behavior is unchanged.

### Optional micro-opt (in scope, low risk)

Pre-size the per-row `Vec<Expr>`: for rows after the first in a multi-row INSERT,
allocate with `Vec::with_capacity(first_row_len)` to avoid 1→N regrowth. The first
row keeps the current growth (arity unknown up front).

### Error cases

No new error paths. Malformed input still flows through `parse_expr` and yields the
same `DbError::ParseError` (same message/position), because the fast-path only
fires on a fully-formed bare-literal-then-delimiter shape.

## Edge cases

Each becomes a test (fast-path **and** the corresponding fall-back must produce
identical AST — assert by parsing and comparing `Stmt`):

- [ ] Single-row all-literal: `VALUES (1, 'a', 18, TRUE, 1.5, 'b')` (fast-path all)
- [ ] Last element before `)` (peek_at(1)=RParen): `VALUES (1)`
- [ ] BigInt boundary: `VALUES (2147483648)` → `BigInt`; `VALUES (2147483647)` → `Int`
- [ ] Hex literal: `VALUES (0xFF)` → `BigInt(255)`
- [ ] Float: `VALUES (3.14)` ; scientific `1e3` if the lexer emits `Float`
- [ ] String with escapes: `VALUES ('a''b')` / `'line\n'` — identical `Text`
- [ ] NULL / TRUE / FALSE elements
- [ ] **Fall-back: compound expr** `VALUES (1 + 2)` → `BinaryOp` (peek_at(1)=`Plus`)
- [ ] **Fall-back: negative literal** `VALUES (-5)` → `UnaryOp(Neg, Literal(5))`
- [ ] **Fall-back: function** `VALUES (NOW())` , `VALUES (CONCAT('a','b'))`
- [ ] **Fall-back: column/ident** `VALUES (col)` , `DEFAULT`
- [ ] **Fall-back: cast / subscript** `VALUES (1::bigint)` , `VALUES (arr[1])`
- [ ] **Fall-back: param** `VALUES (?)` — `param_count` increments correctly
- [ ] **Fall-back: subquery** `VALUES ((SELECT 1))`
- [ ] Multi-row: `VALUES (1,'a'),(2,'b')` — each element fast-pathed; row Vec pre-sized
- [ ] `INSERT ... SET` path unaffected (uses `parse_expr`)
- [ ] String literal `String` not aliased/double-freed (move/clone correctness)

## On-disk format

N/A — parser only, no storage or wire format change.

## Performance budget

Measured with `axiomdb_bench --scenario insert_batch --rows 10000` on macOS
(±60% noisy → medians of ≥3; report `--diagnose-parse` which is low-noise).

| Metric | Before | Target | Max acceptable |
|---|---|---|---|
| AST-build µs/row (`--diagnose-parse`) | ~2.6 | ≤ ~1.2 | ≤ 1.8 |
| full parse µs/row | ~3.4 | ≤ ~2.0 | ≤ 2.8 |
| insert_batch ratio vs SQLite (`--compare`) | ~5.3× | ≤ ~4.2× | < 5.0× |
| SELECT/DDL parse | unchanged | unchanged (≤ +2% noise) | no regression |

Note: full insert parity also needs the executor subphase; this task is the parse
half. No regression on any other parse path is a hard requirement.

## Dependencies

- Depends on: nothing (self-contained parser change).
- Blocks: clean before/after measurement for the executor-trim subphase.

## Open questions

- [x] Handle leading `-`/`+`/`~` or `DEFAULT` in the fast-path? **No** — fall back,
  to guarantee byte-identical AST and keep the guard trivial. (Resolved.)
- [x] Include `HexLit` in the fast set? **Yes** — it is a bare terminal in
  `parse_atom`. (Resolved.)
- [x] Touch the `SET col=val` value parse? **No** — out of the hot VALUES loop and
  lower frequency. (Resolved.)

## Done criteria

- [ ] `parse_value_expr` implemented and used at both VALUES element sites
- [ ] Row `Vec<Expr>` pre-sized for rows after the first
- [ ] Every edge case above has a test asserting fast-path AST == fall-back AST
- [ ] Property/round-trip: a corpus of INSERT statements parses to identical `Stmt`
  before vs after (e.g. compare against `parse_expr`-only via a test shim)
- [ ] `cargo nextest run -p axiomdb-sql` passes (full parser suite green) — Lima VM
- [ ] `cargo nextest run --workspace` passes — Lima VM (subphase close)
- [ ] `cargo clippy --workspace -- -D warnings` clean — Lima VM
- [ ] `cargo fmt --check` clean
- [ ] `--diagnose-parse` shows AST-build within budget; `--compare` insert_batch
  ratio within budget; SELECT/DDL parse not regressed
- [ ] rustdoc on `parse_value_expr`
- [ ] No wire-visible behavior change (parser-only) — note in subphase doc

## References

- Diagnosis: this session's `--diagnose-insert`, `--diagnose-insert-deep`,
  `--diagnose-parse` (added to `benches/comparison/axiomdb_bench/src/main.rs`)
- `crates/axiomdb-sql/src/parser/dml.rs:1829` `parse_insert_body` (VALUES loop ~1893)
- `crates/axiomdb-sql/src/parser/expr.rs:72` `parse_expr`; `:699` `parse_atom`
  (literal conversion `:711–742`); `:581` `parse_unary`
- `crates/axiomdb-sql/src/lexer.rs:70` `Token<'src>` (`Ident`/`QuotedIdent` borrow,
  `StringLit` owns)
- SQLite reference: `research/sqlite/src/insert.c` (VALUES codegen), Lemon grammar
- Checkpoint: `docs/checkpoint-sqlite-parity.md` (writes frontier)
