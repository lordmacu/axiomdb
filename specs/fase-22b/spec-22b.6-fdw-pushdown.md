# Spec: FDW Predicate Pushdown

Phase: 22b — Platform features
Task: 22b.6 — FDW pushdown
Status: approved

## Context

The HTTP FDW (Phase 22b.2) currently fetches all rows from a remote HTTP
endpoint and applies WHERE / LIMIT locally after materialization. This means
`SELECT * FROM remote_users WHERE id = 5` downloads the entire collection and
discards everything except one row.

This spec adds opt-in predicate and LIMIT pushdown: equality predicates and
LIMIT can be forwarded to the remote HTTP server via URL path placeholders or
query parameters, reducing network traffic and remote-side work.

Only the single-table SELECT path (`select_ctx.rs`) gets pushdown. JOIN paths
continue with full scans — cross-table predicate decomposition is out of scope.

## Goal

Allow HTTP FDW tables to forward equality predicates and LIMIT to the remote
server via configurable URL templates and query-parameter mappings, while
guaranteeing correctness via a residual local filter for non-pushed predicates.

## Non-goals

- Range predicates (`>`, `<`, `BETWEEN`) — not pushed (API conventions vary wildly)
- LIKE / IN pushdown — deferred
- JOIN-path pushdown — deferred; join WHERE has cross-table references
- HTTPS support — already deferred since 22b.2
- Verifying that the remote actually filtered (trust remote; correctness guaranteed locally)
- Dynamic predicate routing (only statically-declared columns can be pushed)
- ORDER BY pushdown — deferred

## Behavior

### New TABLE OPTIONS

Three new options extend `CREATE FOREIGN TABLE ... OPTIONS (...)`:

| Option | Type | Description |
|--------|------|-------------|
| `pushdown_cols` | `'col1,col2,...'` | Columns whose equality predicates are appended as `?col=val` query params |
| `limit_param` | `'param_name'` | Query-param name for LIMIT pushdown, e.g. `'limit'` or `'per_page'` |

The existing `endpoint` option is extended: any `{col_name}` occurrence is
treated as a path/query-param placeholder and substituted with the bound
literal value from the WHERE clause.

#### Examples

```sql
-- Path-level pushdown: WHERE id = 5 → GET /users/5
CREATE FOREIGN TABLE users (id INT, name TEXT)
  SERVER myapi OPTIONS (endpoint '/users/{id}');

-- Query-param pushdown: WHERE status = 'active' → GET /orders?status=active
CREATE FOREIGN TABLE orders (id INT, status TEXT, amount FLOAT)
  SERVER myapi OPTIONS (endpoint '/orders', pushdown_cols 'status,customer_id');

-- LIMIT pushdown: LIMIT 10 → GET /products?category=shoes&per_page=10
CREATE FOREIGN TABLE products (id INT, category TEXT, price FLOAT)
  SERVER myapi OPTIONS (
    endpoint '/products/{category}',
    pushdown_cols 'in_stock',
    limit_param 'per_page'
  );
```

### Predicate extraction — `extract_fdw_pushable`

```rust
/// Extracts pushable equality predicates from a WHERE clause.
///
/// Walks AND-connected top-level nodes only. A predicate is pushable when:
///   - It is `Expr::BinaryOp { op: BinaryOp::Eq, left: Column { col_idx }, right: Literal }`
///   - OR the mirror: left is Literal, right is Column
///   - The literal is not NULL (NULL equality is never pushed)
///
/// Returns:
///   - `bound`: map from column name → literal Value for pushable predicates
///   - `residual`: the remaining WHERE expression (None if everything was pushed)
///
/// OR-connected predicates are never split — the whole OR stays in residual.
fn extract_fdw_pushable(
    where_clause: Option<&Expr>,
    columns: &[CatalogColumnDef],  // for col_idx → name resolution
) -> (HashMap<String, Value>, Option<Expr>)
```

**Walking rules:**

| Node shape | Action |
|------------|--------|
| `AND(left, right)` | Recurse into both sides; merge bound maps; rebuild AND from residuals |
| `col = Literal(non-null)` | Push: add to bound map; no residual |
| `Literal(non-null) = col` | Same — commutative |
| `col = Literal(NULL)` | Keep as residual (IS NULL semantics differ) |
| Anything else | Keep as residual |

### URL rendering — `render_fdw_url`

```rust
/// Constructs the final HTTP URL for the FDW request after pushdown.
///
/// Processing order:
///   1. Substitute `{col_name}` placeholders in `endpoint` from `bound`.
///      Unbound placeholders remain literally (→ fallback: URL unchanged).
///   2. For each col in `pushdown_cols` that is in `bound` AND was NOT already
///      consumed by a `{col_name}` placeholder in the endpoint string,
///      append `?col=percent_encoded_value` (or `&col=...` if params exist).
///   3. If `limit_param` is Some and `limit` is Some(n), append the limit param.
///
/// Percent-encoding: encode space→%20, &→%26, =→%3D, +→%2B, #→%23, %→%25.
/// Other ASCII printable chars pass through. Non-ASCII: UTF-8 encode, then
/// percent-encode each byte.
fn render_fdw_url(
    base_url: &str,      // server OPTIONS url
    endpoint: &str,       // table OPTIONS endpoint (may contain {col} placeholders)
    bound: &HashMap<String, Value>,
    pushdown_cols: &[&str],
    limit_param: Option<&str>,
    limit: Option<u64>,
) -> String
```

**Placeholder substitution semantics:**

- Template `{col_name}` is case-sensitive (must match column name exactly).
- A placeholder that matches a key in `bound` is replaced with the
  percent-encoded string representation of the value.
- A placeholder with no match in `bound` is left as-is (literal `{col_name}`
  in the URL). The server will receive the unresolved placeholder string —
  acceptable fallback since the result is still filtered locally.
- A column consumed by a `{col_name}` placeholder in the path is NOT also
  appended as a query param, even if it appears in `pushdown_cols`.

**Value-to-string conversion for URL embedding:**

| AxiomDB type | URL representation |
|---|---|
| `Value::Int(n)` | decimal string, e.g. `"42"` |
| `Value::BigInt(n)` | decimal string |
| `Value::Real(f)` | shortest decimal representation (no trailing zeros) |
| `Value::Text(s)` | percent-encoded |
| `Value::Bool(b)` | `"true"` / `"false"` |
| `Value::Null` | never pushed (filtered out in extraction) |

### Updated `fdw_scan_table` signature

```rust
fn fdw_scan_table(
    storage: &dyn StorageEngine,
    snap: axiomdb_core::TransactionSnapshot,
    table_id: u32,
    columns: &[CatalogColumnDef],
    pushed_predicates: &HashMap<String, Value>,  // from extract_fdw_pushable
    limit: Option<u64>,                           // from stmt.limit if Literal
) -> Result<Vec<(RecordId, crate::result::Row)>, DbError>
```

Internally reads `pushdown_cols` and `limit_param` from the table's OPTIONS,
then calls `render_fdw_url`, then `http_get`.

### Integration into single-table SELECT path (`select_ctx.rs`)

Current code (lines ~100–116):

```rust
if resolved.def.id >= FOREIGN_TABLE_ID_BASE {
    let fdw_rows = fdw_scan_table(storage, snap, resolved.def.id, &resolved.columns)?;
    ...
    return execute_select_with_joins_first_materialized(stmt, ...);
}
```

New code:

```rust
if resolved.def.id >= FOREIGN_TABLE_ID_BASE {
    let (pushed, residual_where) =
        extract_fdw_pushable(stmt.where_clause.as_ref(), &resolved.columns);

    let pushed_limit: Option<u64> = stmt.limit.as_ref().and_then(|e| match e {
        Expr::Literal(Value::Int(n)) if *n >= 0 => Some(*n as u64),
        Expr::Literal(Value::BigInt(n)) if *n >= 0 => Some(*n as u64),
        _ => None,
    });

    let fdw_rows = fdw_scan_table(
        storage, snap, resolved.def.id, &resolved.columns,
        &pushed, pushed_limit,
    )?;

    // Replace WHERE with residual only — pushed predicates handled by remote.
    let mut stmt2 = stmt.clone();
    stmt2.where_clause = residual_where;
    // Do NOT strip stmt2.limit — executor still enforces it locally (idempotent).

    return execute_select_with_joins_first_materialized(stmt2, ...);
}
```

### Join-path call sites (no change in behavior)

Both call sites in `select_joins_ctx.rs` pass `&HashMap::new()` and `None`:

```rust
let rows = fdw_scan_table(
    storage, snap.clone(), jt.def.id, &jt.columns,
    &HashMap::new(), None,
)?;
```

No pushdown occurs for FDW tables in JOIN position. WHERE and LIMIT are still
applied locally by the join executor. This is safe and correct.

### Error cases

| Condition | Expected error | Notes |
|-----------|----------------|-------|
| `pushdown_cols` value cannot be parsed (not comma-separated strings) | `DbError::InvalidValue` | message: `"FDW: invalid pushdown_cols option: ..."` |
| HTTP connect fails (same as before) | `DbError::Internal` | unchanged |
| Remote returns non-JSON-array (same as before) | `DbError::Internal` | unchanged |
| Placeholder `{col}` in endpoint references a column not in the table schema | silently left as-is | not an error; treated as unbound |

## Edge cases

- [ ] No pushdown configured (no placeholders, no `pushdown_cols`, no `limit_param`) — URL unchanged; behavior identical to 22b.2
- [ ] `pushdown_cols` column not present in WHERE — column skipped, not appended
- [ ] `WHERE pushed_col = X AND non_pushed_col = Y` — only `pushed_col` goes to URL; `non_pushed_col = Y` stays as residual and is applied locally
- [ ] `WHERE pushed_col IS NULL` — IS NULL never pushed; stays as residual
- [ ] `WHERE pushed_col = X OR other = Y` — OR is not split; entire predicate stays as residual
- [ ] `WHERE pushed_col = X AND pushed_col = Z` (same col, two equalities) — both are pushed; remote receives `?pushed_col=X&pushed_col=Z` (last wins depends on remote, but local residual is empty so both values are filtered locally only if remote misbehaves — acceptable)
- [ ] Endpoint has `{id}` placeholder; WHERE has no `id =` predicate — `{id}` left as literal in URL; full result filtered locally
- [ ] `LIMIT 0` — pushed as `?limit_param=0`; valid
- [ ] `LIMIT` is an expression (not a literal integer) — not pushed; local LIMIT applied only
- [ ] Column name with special chars in query param (e.g. column named `user-id`) — percent-encoded in URL value, column name used as-is in param name (AxiomDB column names are ASCII identifiers, safe for query param keys)
- [ ] `pushdown_cols` lists a column that also appears as `{col}` placeholder — placeholder takes priority; query param not duplicated
- [ ] Value of type `Real` that is NaN or Infinity — render as `"NaN"` / `"Infinity"`; no error

## Performance budget

| Scenario | Expected improvement | Notes |
|----------|---------------------|-------|
| `WHERE id = X` with `endpoint '/items/{id}'` | Remote returns 1 row instead of N | Eliminates full collection transfer |
| `LIMIT 10` with `limit_param 'limit'` | Remote returns 10 rows instead of N | Depends on remote honouring the param |
| No pushdown configured | Zero overhead vs 22b.2 | `render_fdw_url` is a string op, < 1µs |

The FDW scan is inherently network-bound. No latency regression budget needed
for the local path — the string manipulation is negligible.

## Dependencies

- Depends on: Phase 22b.2 (HTTP FDW), 22b.4 (schema namespacing — `SelectStmt.clone()`)
- Blocks: nothing in 22b

## Open questions

None — all resolved in brainstorm.

## Done criteria

- [ ] `extract_fdw_pushable` correctly splits AND-connected equality predicates from residuals
- [ ] `render_fdw_url` substitutes `{col}` placeholders and appends `pushdown_cols` params
- [ ] `render_fdw_url` appends `limit_param` when `limit` is `Some`
- [ ] `fdw_scan_table` accepts `pushed_predicates` and `limit` params
- [ ] Single-table SELECT path extracts pushable predicates and passes them to `fdw_scan_table`
- [ ] Residual WHERE is preserved and applied locally
- [ ] `LIMIT` is always applied locally regardless of pushdown
- [ ] Join-path call sites compile with `&HashMap::new()` and `None` (no behavior change)
- [ ] All existing Phase 22b.2 FDW integration tests pass unchanged
- [ ] New integration tests (≥6) with a URL-capturing mock HTTP server verify:
  - path placeholder substitution
  - query-param pushdown via `pushdown_cols`
  - LIMIT pushdown via `limit_param`
  - mixed: some predicates pushed, residual filtered locally
  - OR predicate: not pushed, filtered locally
  - no pushdown config: URL unchanged
- [ ] `cargo nextest run -p axiomdb-sql` passes on Lima VM
- [ ] `cargo clippy -p axiomdb-sql -- -D warnings` clean
- [ ] `cargo fmt --check` clean

## References

- Phase 22b.2 implementation: `crates/axiomdb-sql/src/executor/fdw_http.rs`
- Entry points: `select_ctx.rs:100`, `select_joins_ctx.rs:14,92`
- PostgreSQL FDW qual pushdown: `src/backend/foreign/fdwapi.c` — `GetForeignPlan` + `fdw_exprs`
- REST query-param convention reference: OpenAPI `style: form, explode: true`
