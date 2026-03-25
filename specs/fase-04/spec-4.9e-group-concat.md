# Spec: 4.9e — GROUP_CONCAT

## What to build (not how)

Implement `GROUP_CONCAT([DISTINCT] expr [ORDER BY expr [ASC|DESC] [, ...]] [SEPARATOR 'str'])`
as a first-class aggregate function that works inside `GROUP BY` queries and ungrouped queries.

`IF`, `IFNULL`, `NULLIF` are not in scope. `string_agg` (PostgreSQL-compatible alias) IS in scope.

---

## Inputs / Outputs

### Signature

```sql
GROUP_CONCAT([DISTINCT] expr [ORDER BY sort_expr [ASC|DESC] [, ...]] [SEPARATOR 'str'])
```

| Component   | Type        | Default | Notes                          |
|-------------|-------------|---------|--------------------------------|
| `expr`      | any         | —       | Value to concatenate (coerced to TEXT) |
| `DISTINCT`  | flag        | false   | Deduplicate values before concat |
| `ORDER BY`  | clause      | none    | Sort values before concat (multi-col) |
| `SEPARATOR` | TEXT literal| `','`   | String placed between values   |

- **Output:** `Value::Text` — the concatenated string
- **NULL input values:** skipped (not included in output)
- **Empty group (no non-NULL values):** returns `Value::Null`
- **SEPARATOR NULL:** treated as empty string `''`

### `string_agg(expr, separator)` alias

PostgreSQL-compatible 2-arg form: `string_agg(col, ', ')`. No ORDER BY or DISTINCT.
Equivalent to `GROUP_CONCAT(col SEPARATOR ', ')`.

---

## Use cases

### 1. Basic GROUP_CONCAT

```sql
SELECT post_id, GROUP_CONCAT(tag) FROM post_tags GROUP BY post_id;
-- post 1: 'rust,db,async'  (comma-separated, order undefined)
```

### 2. Custom separator

```sql
SELECT post_id, GROUP_CONCAT(tag SEPARATOR ', ') FROM post_tags GROUP BY post_id;
-- post 1: 'rust, db, async'
```

### 3. ORDER BY inside aggregate

```sql
SELECT post_id, GROUP_CONCAT(tag ORDER BY tag ASC SEPARATOR ', ')
FROM post_tags GROUP BY post_id;
-- post 1: 'async, db, rust'  (sorted)
```

### 4. DISTINCT

```sql
SELECT GROUP_CONCAT(DISTINCT category SEPARATOR ' | ')
FROM articles GROUP BY author_id;
-- deduplicates categories per author
```

### 5. DISTINCT + ORDER BY

```sql
SELECT GROUP_CONCAT(DISTINCT tag ORDER BY tag ASC) FROM post_tags GROUP BY post_id;
-- deduplicates first, then sorts
```

### 6. Ungrouped query (implicit single group)

```sql
SELECT GROUP_CONCAT(name ORDER BY name SEPARATOR ', ') FROM users;
-- all names in one string, sorted
```

### 7. NULL handling

```sql
-- Given: post_id=1 has tags: 'rust', NULL, 'db'
SELECT GROUP_CONCAT(tag) FROM post_tags WHERE post_id = 1;
-- 'rust,db'  (NULL skipped)
```

### 8. All-NULL group

```sql
SELECT GROUP_CONCAT(tag) FROM post_tags WHERE post_id = 999;
-- NULL  (no rows, empty group)
```

### 9. string_agg alias

```sql
SELECT string_agg(name, ', ') FROM users;
-- equivalent to GROUP_CONCAT(name SEPARATOR ', ')
```

### 10. Multi-column ORDER BY

```sql
SELECT GROUP_CONCAT(name ORDER BY dept ASC, name DESC SEPARATOR ';')
FROM employees GROUP BY manager_id;
```

---

## Acceptance criteria

- [ ] `GROUP_CONCAT(col)` returns comma-separated values (order undefined, matches accumulation)
- [ ] `GROUP_CONCAT(col SEPARATOR ' | ')` uses the specified separator
- [ ] `GROUP_CONCAT(col ORDER BY col ASC)` returns values in sorted order
- [ ] `GROUP_CONCAT(col ORDER BY col DESC)` returns values in reverse sorted order
- [ ] `GROUP_CONCAT(col ORDER BY col2 ASC, col3 DESC)` multi-column ORDER BY works
- [ ] `GROUP_CONCAT(DISTINCT col)` deduplicates values
- [ ] `GROUP_CONCAT(DISTINCT col ORDER BY col ASC)` combines DISTINCT + ORDER BY
- [ ] NULL values in `col` are skipped (not included in output)
- [ ] Group with no non-NULL values returns `Value::Null`
- [ ] `string_agg(col, sep)` is a 2-arg alias that works identically to `GROUP_CONCAT(col SEPARATOR sep)`
- [ ] Works in ungrouped queries (implicit single group)
- [ ] Works in `HAVING`: `HAVING GROUP_CONCAT(tag) LIKE '%rust%'` (aggregated value filterable)
- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy -- -D warnings` clean
- [ ] Wire protocol smoke test passes

---

## Edge cases

| Input | Expected |
|-------|----------|
| Empty group | `NULL` |
| All-NULL group | `NULL` |
| Single non-NULL value | value itself (no separator) |
| Value = empty string `''` | concatenated as empty (separator + '' = separator still appears) |
| Separator = `''` | values joined with no separator |
| `DISTINCT` with all-same values | single value in result |
| Very long result (> 1 MB) | truncated to 1,048,576 bytes (MySQL default `group_concat_max_len`) |

---

## Out of scope

- `GROUP_CONCAT(col LIMIT n)` — MariaDB extension, not MySQL standard
- `GROUP_CONCAT(col1, col2)` — multi-column concatenation (MariaDB extension)
- `group_concat_max_len` as a session variable (fixed at 1MB)
- Window function variant (`GROUP_CONCAT(...) OVER (PARTITION BY ...)`) — Phase 9
- `WITHIN GROUP (ORDER BY ...)` PostgreSQL syntax (not MySQL-compatible)

---

## Dependencies

- `SortOrder` enum already in `ast.rs` — reuse
- `OrderByItem` already in `ast.rs` — reuse for ORDER BY inside GROUP_CONCAT
- `is_aggregate()` in `executor.rs` — extend
- `AggExpr` struct in `executor.rs` — convert to enum
- `AggAccumulator` enum in `executor.rs` — add `GroupConcat` variant
- New `Expr::GroupConcat` variant in `expr.rs` — required for parser round-trip

---

## Research sources

- MariaDB: `sql/item_sum.cc` — `Item_func_group_concat::add()` (lines 4204–4278),
  `Item_func_group_concat::val_str()` (lines 4479–4505), class definition in `item_sum.h`
  - Key insight: uses B+tree only when ORDER BY present; direct append otherwise
- SQLite: `src/func.c` — `groupConcatStep` (lines 2184–2252): simple StrAccum buffer,
  separator prepended before each value except first
- DuckDB: `string_agg.cpp` — arena-based buffer, separator is a bind-time constant
- PostgreSQL: `varlena.c` — `string_agg_transfn`: separator stored first, stripped in finalize
- **Consensus across all 4 systems:**
  - Separator placed BEFORE each value except the first
  - NULL values silently skipped
  - Empty group → NULL, not empty string
