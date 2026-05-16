# Spec: 20.19 — ltree hierarchical path type

Phase: 20 — Types + import/export
Task: 20.19 within the sprint
Status: approved

## Context

AxiomDB Phase 20 adds user-defined and composite types. Phase 20.18 added composite types;
20.19 adds `ltree`, a PostgreSQL-compatible hierarchical label-path type used for tree
structures (org charts, file paths, category taxonomies, DNS zones). The type lives in
`axiomdb-types` (value/codec), `axiomdb-catalog` (ColumnType), and `axiomdb-sql`
(parser, analyzer, executor, wire).

## Goal

Deliver a fully functional `LTREE` column type with path validation, 4 operators
(`@>`, `<@`, `~`, `||`), 7 scalar functions (`nlevel`, `subpath`, `subltree`,
`index`, `lca`, `text2ltree`, `ltree2text`), and wire-protocol text serialization.

## Non-goals

- GIN / GiST indexes for O(log n) subtree queries — deferred to Phase 28.
- `ltxtquery` fulltext search on ltree labels.
- lquery quantifiers (`{n,m}`, `@` case-insensitive label modifier, `*{n,m}`).
- `GENERATED ALWAYS AS` computed ltree columns.
- `nlevel` as a column constraint or index expression.

## Behavior

### Ltree path format

An ltree path is a sequence of **labels** separated by dots (`.`):

```
electronics.phones.smartphones
a
foo.bar.baz.qux
```

**Label validation:**
- Each label matches `[A-Za-z0-9_]+` (ASCII letters, digits, underscore).
- Minimum 1 label; a path cannot be empty.
- Maximum label length: 255 bytes.
- Maximum path length: 65 535 bytes.
- No leading or trailing dot; no consecutive dots.

### Public API

```rust
// axiomdb-types/src/value.rs
pub enum Value {
    // ... existing variants ...
    Ltree(String),   // validated ltree path string
}

// axiomdb-types/src/types.rs
pub enum DataType {
    // ... existing variants ...
    Ltree,
}

// axiomdb-catalog/src/schema_database.rs
pub enum ColumnType {
    // ... existing variants (Composite = 16) ...
    Ltree = 17,
}

// axiomdb-types/src/ltree.rs  (new file)
/// Validate that `s` is a well-formed ltree path.
pub fn validate_ltree_path(s: &str) -> Result<(), DbError>;

/// Return true if `ancestor` is a prefix of (or equal to) `path`.
pub fn ltree_is_ancestor(ancestor: &str, path: &str) -> bool;

/// Concatenate two ltree paths with a dot separator.
pub fn ltree_concat(left: &str, right: &str) -> String;

/// Match `path` against an lquery pattern.
/// Pattern labels: literal label (exact match) or `*` (0+ labels).
pub fn lquery_match(path: &str, pattern: &str) -> bool;

/// Return the number of labels in `path`.
pub fn ltree_nlevel(path: &str) -> usize;

/// Return a sub-path starting at `offset` (0-based) with optional `len`.
/// Returns None if offset is out of range.
pub fn ltree_subpath(path: &str, offset: usize, len: Option<usize>) -> Option<String>;

/// Return position (0-based) of `subpath` within `path`, starting search at `offset`.
/// Returns None if not found.
pub fn ltree_index(path: &str, subpath: &str, offset: usize) -> Option<usize>;

/// Return the longest common ancestor of two or more paths.
/// Returns empty string if there is no common ancestor.
pub fn ltree_lca(paths: &[&str]) -> String;
```

### SQL surface

```sql
-- Column type
CREATE TABLE categories (id INT, path LTREE);

-- Insert via text literal (implicit cast)
INSERT INTO categories VALUES (1, 'electronics.phones');
INSERT INTO categories VALUES (2, 'electronics.phones.smartphones');
INSERT INTO categories VALUES (3, 'electronics.laptops');

-- Ancestor operator: is 'electronics' an ancestor of the path?
SELECT path FROM categories WHERE 'electronics' @> path;
-- returns all 3 rows

-- Descendant operator: is the path a descendant of 'electronics.phones'?
SELECT path FROM categories WHERE path <@ 'electronics.phones';
-- returns rows 1 and 2

-- lquery pattern match
SELECT path FROM categories WHERE path ~ 'electronics.*';
-- returns all 3 rows (any path under electronics)

SELECT path FROM categories WHERE path ~ '*.smartphones';
-- returns row 2

-- Concatenation
SELECT 'electronics' || '.' || 'audio';           -- ERROR: || on text
SELECT 'electronics'::LTREE || 'audio'::LTREE;    -- 'electronics.audio'

-- Functions
SELECT nlevel('a.b.c');                 -- 3
SELECT subpath('a.b.c.d', 1);          -- 'b.c.d'
SELECT subpath('a.b.c.d', 1, 2);       -- 'b.c'
SELECT subltree('a.b.c.d', 0, 2);      -- 'a.b'
SELECT index('a.b.c.a.b', 'a.b');      -- 0
SELECT index('a.b.c.a.b', 'a.b', 1);   -- 3
SELECT lca('a.b.c', 'a.b.d');          -- 'a.b'
SELECT lca('a.b', 'c.d');              -- '' (no common ancestor)
SELECT text2ltree('a.b.c');            -- 'a.b.c' (validated)
SELECT ltree2text('a.b.c'::LTREE);     -- 'a.b.c' (as TEXT)
```

### Operator semantics

| Operator | Types | Result | Description |
|----------|-------|--------|-------------|
| `@>` | `ltree @> ltree` | BOOL | left is ancestor-or-equal of right |
| `<@` | `ltree <@ ltree` | BOOL | left is descendant-or-equal of right |
| `~`  | `ltree ~ text`  | BOOL | left path matches lquery pattern |
| `\|\|` | `ltree \|\| ltree` | LTREE | concatenate paths with `.` |

**`@>` (ancestor-or-equal):**
- `a @> b` iff `b` starts with `a` as a label prefix.
- `'a.b' @> 'a.b.c'` → true (`a.b` is ancestor of `a.b.c`)
- `'a.b' @> 'a.b'`   → true (equal paths)
- `'a.b' @> 'a.bc'`  → false (label `bc` ≠ `b`)
- `'a.b' @> 'a'`     → false (shorter path is not an ancestor)

**`<@` (descendant-or-equal):**
- `a <@ b` iff `b @> a` (i.e., `b` is ancestor of `a`)

**`~` (lquery pattern):**
- Pattern labels separated by `.`; `*` matches zero or more labels.
- `'a.b.c' ~ '*'` → true (matches any path)
- `'a.b.c' ~ 'a.*'` → true
- `'a.b.c' ~ '*.b.*'` → true
- `'a.b.c' ~ 'a.b'` → false (must match the entire path)
- `'a.b.c' ~ 'a.*.c'` → true
- Pattern must match the **entire** path (implicit anchored match).

**`||` (concatenation):**
- `'a.b' || 'c.d'` → `'a.b.c.d'`
- Only dispatches to ltree concat when **both** operands are `Value::Ltree`.
- `Value::Text || Value::Text` still goes to string concat.

### Error cases

| Input | Expected error | Condition |
|-------|----------------|-----------|
| `CAST('a..b' AS LTREE)` | `DbError::InvalidValue` | consecutive dots |
| `CAST('.a' AS LTREE)` | `DbError::InvalidValue` | leading dot |
| `CAST('a.' AS LTREE)` | `DbError::InvalidValue` | trailing dot |
| `CAST('a b' AS LTREE)` | `DbError::InvalidValue` | space in label |
| `CAST('' AS LTREE)` | `DbError::InvalidValue` | empty path |
| `CAST('a.b!c' AS LTREE)` | `DbError::InvalidValue` | invalid char `!` |
| `subpath('a.b', 5)` | `DbError::InvalidValue` | offset out of range |
| `subpath('a.b', 0, 10)` | returns `'a.b'` | len exceeds, clips to end |
| `text2ltree('bad!')` | `DbError::InvalidValue` | same as CAST |
| `lca()` (0 args) | `DbError::InvalidValue` | requires at least 1 arg |
| `index('a.b', 'c', -1)` | `DbError::InvalidValue` | negative offset |

### lquery_match algorithm

Split path on `.` into `path_parts[0..n]`.
Split pattern on `.` into `pat_parts[0..m]`.

Use a two-pointer greedy algorithm:
- Walk pattern left to right.
- When `pat_parts[j] == "*"`, greedily skip 0–(remaining_path_labels) labels.
- Otherwise, require exact case-sensitive match of the label.
- The entire path must be consumed when the entire pattern is consumed.

Edge cases:
- `*` at start of pattern matches empty prefix.
- `*` at end matches any remaining labels.
- Multiple consecutive `*` tokens are treated as a single `*`.
- Pattern `*` alone matches any non-empty path.

### On-disk format

```
Byte layout for Ltree column in a row:
  offset  size  field        description
  0       4     data_len     u32 LE — byte length of the UTF-8 path string
  4       N     path_bytes   UTF-8 bytes of the path (not NUL-terminated)
```

Same encoding as `Text` (length-prefix + UTF-8). Validation is enforced on write only;
on read, the stored bytes are trusted.

### Wire protocol

- MySQL type: `0xfd` (VAR_STRING)
- Charset: `results_collation.id` (text, not binary 63)
- Serialized as: the path string itself (e.g., `"electronics.phones"`)
- `datatype_to_mysql_type(DataType::Ltree)` → `0xfd`
- `column_display_len(DataType::Ltree)` → `65_535`

## Edge cases

- [ ] Single-label path: `'a'` (nlevel=1, subpath valid, ancestors={`'a'`})
- [ ] `@>` / `<@` with identical paths → true (ancestor-or-equal)
- [ ] `~` with pattern `*` matches any valid path
- [ ] `||` of two single-label paths: `'a' || 'b'` → `'a.b'`
- [ ] `lca` of identical paths returns the path itself
- [ ] `lca` of disjoint paths returns `''` (empty string as Value::Ltree)
- [ ] `subpath` with `offset = 0, len = nlevel` returns full path
- [ ] `index` not found returns `-1` (as `Value::Int(-1)`)
- [ ] `subpath` offset exactly at last label returns single-label path
- [ ] NULL ltree column: operators propagate NULL; functions return NULL
- [ ] INSERT ltree value from SELECT (e.g., `INSERT INTO t SELECT path FROM other`)
- [ ] WHERE `path ~ pattern` where pattern is a column (not literal) — should work

## Performance budget

| Operation | Target | Max acceptable |
|-----------|--------|----------------|
| `@>` / `<@` on table scan (100k rows) | < 50 ms | < 200 ms |
| `lquery_match` per call | < 1 µs | < 5 µs |

No indexed path needed at this phase. Full-table-scan behavior expected.

## Dependencies

- Depends on: Phase 20.18 complete (Composite=16 assigned; next slot is Ltree=17)
- Blocks: nothing in Phase 20

## Open questions

All resolved:
- `*` alone in lquery matches any non-empty path ✓
- `lca` of empty paths: return `Value::Ltree("".into())` (empty string) ✓
- `index` not found: return `Value::Int(-1)` (PostgreSQL convention) ✓
- `||` dispatches on `Value::Ltree` only, not `Value::Text` ✓

## Done criteria

- [ ] `Value::Ltree(String)`, `DataType::Ltree`, `ColumnType::Ltree=17` exist
- [ ] `validate_ltree_path` rejects all invalid paths listed in Error cases
- [ ] `encode_row` / `decode_row` round-trip `Value::Ltree` correctly
- [ ] `CREATE TABLE t (path LTREE)` succeeds and persists the column type
- [ ] `INSERT INTO t VALUES ('a.b.c')` inserts (implicit text→ltree coerce)
- [ ] `CAST('a.b.c' AS LTREE)` works; `CAST('bad!' AS LTREE)` returns `InvalidValue`
- [ ] All 4 operators (`@>`, `<@`, `~`, `||`) produce correct results
- [ ] All 7 functions (`nlevel`, `subpath`, `subltree`, `index`, `lca`, `text2ltree`, `ltree2text`) produce correct results
- [ ] NULL propagation: all operators and functions return NULL when input is NULL
- [ ] ≥ 15 integration tests in `crates/axiomdb-sql/tests/integration_ltree.rs`
- [ ] ≥ 4 wire assertions `[20.19 ltree]` in `tools/wire-test.py`
- [ ] `cargo nextest run --workspace` passes (all existing + new tests)
- [ ] `cargo clippy --workspace -- -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] `docs/progreso.md` updated: 20.19 marked `[x] ✅`
- [ ] `docs/fase-20.md` updated with 20.19 section

## References

- PostgreSQL ltree docs: https://www.postgresql.org/docs/current/ltree.html
- Related spec: `specs/fase-20/spec-range-types.md` (same pattern: new Value variant + ColumnType)
- Phase 20.15 regex: `specs/fase-20/spec-regex-operators.md` (~ operator precedent)
- Phase 20.18 composite: `specs/fase-20/spec-20.18-composite-types.md` (ColumnType=16 precedes)
