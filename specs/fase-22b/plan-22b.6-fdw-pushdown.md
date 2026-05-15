# Plan: FDW Predicate Pushdown

Phase: 22b — Platform features
Task: 22b.6 — FDW pushdown
Spec: specs/fase-22b/spec-22b.6-fdw-pushdown.md
Status: in-progress

## Summary

All changes live in `crates/axiomdb-sql`. No new crates or dependencies.

Steps follow TDD order. Steps 1 and 2 build and test the two pure helper
functions in isolation. Step 3 wires them into `fdw_scan_table` and updates
all three call sites to compile with the new signature. Step 4 adds the
predicate-extraction logic into the single-table SELECT path. Step 5 adds
URL-capturing integration tests that verify what URL actually reaches the
mock HTTP server.

Order rationale: helpers first (Steps 1-2) so the compiler validates the
logic before anything touches the executor. Step 3 (signature update) must
come before Step 4 (call-site wiring) because `select_ctx.rs` calls
`fdw_scan_table`. Step 5 (integration) comes last when everything is wired.

## Dependencies

Must be done first:
- [x] spec-22b.6-fdw-pushdown.md approved
- [x] Phase 22b.2 HTTP FDW implemented (`fdw_http.rs` exists)

Blocks:
- nothing in 22b

## Affected files

Modified files:
- `crates/axiomdb-sql/src/executor/fdw_http.rs` — Steps 1, 2, 3 (add helpers + new signature)
- `crates/axiomdb-sql/src/executor/select_ctx.rs` — Step 4 (wire pushdown into single-table path)
- `crates/axiomdb-sql/src/executor/select_joins_ctx.rs` — Step 3 (update two call sites, no behavior change)
- `crates/axiomdb-sql/tests/integration_fdw.rs` — Step 5 (new URL-capturing tests)

---

## Step 1 — `extract_fdw_pushable` + unit tests

**Goal:** Pure function that splits a WHERE clause into pushable equality
predicates and a residual expression.

**Files:** `crates/axiomdb-sql/src/executor/fdw_http.rs`

**Approach:** TDD — write the unit tests as inline `#[cfg(test)]` mod, then
implement. The function only reads the `Expr` tree and the column slice —
no I/O, no storage.

### Implementation outline

```rust
// Inside fdw_http.rs

use std::collections::HashMap;
use axiomdb_types::Value;
use crate::expr::{BinaryOp, Expr};

/// Splits `where_clause` into:
/// - `bound`: col_name → literal Value for AND-connected `col = literal` leaves
/// - residual: the rest of the WHERE tree (None if everything was pushed)
///
/// Only `BinaryOp::Eq` with one Column and one non-NULL Literal side qualifies.
/// OR nodes, IS NULL, BETWEEN, functions, etc. are always residual.
fn extract_fdw_pushable(
    where_clause: Option<&Expr>,
    columns: &[CatalogColumnDef],
) -> (HashMap<String, Value>, Option<Expr>) {
    let Some(expr) = where_clause else {
        return (HashMap::new(), None);
    };
    extract_expr(expr, columns)
}

fn extract_expr(
    expr: &Expr,
    columns: &[CatalogColumnDef],
) -> (HashMap<String, Value>, Option<Expr>) {
    match expr {
        Expr::BinaryOp { op: BinaryOp::And, left, right } => {
            let (mut bl, rl) = extract_expr(left, columns);
            let (br, rr) = extract_expr(right, columns);
            bl.extend(br);
            let residual = match (rl, rr) {
                (None, None) => None,
                (Some(l), None) => Some(l),
                (None, Some(r)) => Some(r),
                (Some(l), Some(r)) => Some(Expr::BinaryOp {
                    op: BinaryOp::And,
                    left: Box::new(l),
                    right: Box::new(r),
                }),
            };
            (bl, residual)
        }
        Expr::BinaryOp { op: BinaryOp::Eq, left, right } => {
            if let Some((col_name, val)) = try_eq_push(left, right, columns) {
                let mut m = HashMap::new();
                m.insert(col_name, val);
                return (m, None);
            }
            (HashMap::new(), Some(expr.clone()))
        }
        _ => (HashMap::new(), Some(expr.clone())),
    }
}

/// Returns (col_name, value) if one side is a Column ref and the other is a
/// non-NULL Literal. Returns None otherwise.
fn try_eq_push(
    left: &Expr,
    right: &Expr,
    columns: &[CatalogColumnDef],
) -> Option<(String, Value)> {
    match (left, right) {
        (Expr::Column { col_idx, .. }, Expr::Literal(v)) if !v.is_null() => {
            let name = columns.get(*col_idx)?.name.clone();
            Some((name, v.clone()))
        }
        (Expr::Literal(v), Expr::Column { col_idx, .. }) if !v.is_null() => {
            let name = columns.get(*col_idx)?.name.clone();
            Some((name, v.clone()))
        }
        _ => None,
    }
}
```

### Tests to add

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use axiomdb_catalog::schema_table::ColumnDef as CatalogColumnDef;
    use axiomdb_types::Value;
    use crate::expr::{BinaryOp, Expr};

    fn make_cols(names: &[&str]) -> Vec<CatalogColumnDef> { ... }
    fn col(idx: usize, name: &str) -> Expr { ... }
    fn lit_int(n: i32) -> Expr { Expr::Literal(Value::Int(n)) }
    fn lit_text(s: &str) -> Expr { Expr::Literal(Value::Text(s.into())) }
    fn lit_null() -> Expr { Expr::Literal(Value::Null) }

    #[test] fn extract_none_where()             // None → (empty, None)
    #[test] fn extract_single_eq()              // col=1 → ({col→1}, None)
    #[test] fn extract_reversed_eq()            // 1=col → ({col→1}, None)
    #[test] fn extract_null_eq_stays_residual() // col=NULL → ({}, Some(col=NULL))
    #[test] fn extract_and_both_pushable()      // col1=1 AND col2=2 → ({col1→1,col2→2}, None)
    #[test] fn extract_and_mixed()              // col1=1 AND col1>5 → ({col1→1}, Some(col1>5))
    #[test] fn extract_or_stays_residual()      // col=1 OR other=2 → ({}, Some(entire OR))
    #[test] fn extract_is_null_stays_residual() // IS NULL stays as-is
    #[test] fn extract_deep_and_chain()         // a=1 AND b=2 AND c=3 → all pushed
}
```

### Verification

```bash
./tools/vm.sh test axiomdb-sql --test-threads=1 2>&1 | tail -5
```

(Run on Lima VM. Only the unit test module — no HTTP calls needed.)

### Commit

```
feat(fase-22b): add extract_fdw_pushable — predicate splitter for FDW pushdown

Step 1 of specs/fase-22b/plan-22b.6-fdw-pushdown.md
```

---

## Step 2 — `render_fdw_url` + unit tests

**Goal:** Pure function that constructs the final HTTP URL from the endpoint
template, bound predicates, pushdown_cols config, and limit.

**Files:** `crates/axiomdb-sql/src/executor/fdw_http.rs`

### Implementation outline

```rust
/// Constructs the HTTP URL for an FDW GET request with predicate/limit pushdown.
///
/// See spec for full semantics.
fn render_fdw_url(
    base_url: &str,
    endpoint: &str,
    bound: &HashMap<String, Value>,
    pushdown_cols: &[&str],
    limit_param: Option<&str>,
    limit: Option<u64>,
) -> String {
    // 1. Substitute {col_name} placeholders in endpoint.
    //    Track which columns were consumed by placeholders.
    let mut consumed: HashSet<&str> = HashSet::new();
    let rendered_endpoint = substitute_placeholders(endpoint, bound, &mut consumed);

    // 2. Collect query params: pushdown_cols that are bound and not consumed.
    let mut params: Vec<(String, String)> = Vec::new();
    for &col in pushdown_cols {
        if !consumed.contains(col) {
            if let Some(val) = bound.get(col) {
                params.push((col.to_string(), value_to_url_string(val)));
            }
        }
    }

    // 3. LIMIT param.
    if let (Some(param), Some(n)) = (limit_param, limit) {
        params.push((param.to_string(), n.to_string()));
    }

    // 4. Build final URL.
    let base = format!(
        "{}{}",
        base_url.trim_end_matches('/'),
        rendered_endpoint
    );
    if params.is_empty() {
        return base;
    }
    let sep = if base.contains('?') { '&' } else { '?' };
    let qs: String = params
        .iter()
        .enumerate()
        .map(|(i, (k, v))| {
            let delim = if i == 0 { sep.to_string() } else { "&".to_string() };
            format!("{delim}{}={}", k, percent_encode(v))
        })
        .collect();
    format!("{base}{qs}")
}

/// Substitute {col} placeholders; record consumed column names.
fn substitute_placeholders(
    endpoint: &str,
    bound: &HashMap<String, Value>,
    consumed: &mut HashSet<&str>,  // borrows keys from bound
) -> String { ... }

/// Percent-encode: space→%20, &→%26, =→%3D, +→%2B, #→%23, %→%25.
/// Non-ASCII bytes percent-encoded. ASCII printable (except above) pass through.
fn percent_encode(s: &str) -> String { ... }

/// Convert an AxiomDB Value to its URL string representation.
fn value_to_url_string(v: &Value) -> String {
    match v {
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::Real(f) => format_real_url(*f),
        Value::Text(s) => s.clone(),          // caller percent-encodes
        Value::Bool(b) => if *b { "true" } else { "false" }.into(),
        Value::Null => String::new(),          // unreachable; NULL never pushed
    }
}

fn format_real_url(f: f64) -> String {
    if f.is_nan() { return "NaN".into(); }
    if f.is_infinite() { return if f > 0.0 { "Infinity" } else { "-Infinity" }.into(); }
    // Drop trailing zeros
    let s = format!("{f}");
    s
}
```

### Tests to add (inline `#[cfg(test)]`)

```rust
#[test] fn render_no_pushdown()          // no placeholders, no cols → base+endpoint unchanged
#[test] fn render_path_placeholder()     // endpoint '/users/{id}', bound {id→5} → /users/5
#[test] fn render_unbound_placeholder()  // endpoint '/users/{id}', no id in WHERE → /users/{id}
#[test] fn render_query_param()          // pushdown_cols ['status'], bound {status→active} → ?status=active
#[test] fn render_limit_param()          // limit_param 'limit', limit=10 → ?limit=10
#[test] fn render_mixed_path_and_query() // {cat} in path + pushdown col 'brand' → /cat/shoes?brand=nike
#[test] fn render_placeholder_not_duplicated_in_query() // {id} in path AND in pushdown_cols → only in path
#[test] fn render_percent_encode_spaces() // ' hello world' → %20hello%20world
#[test] fn render_percent_encode_ampersand() // 'a&b' → a%26b
#[test] fn render_existing_query_string() // endpoint '/u?v=1', param appended with &
#[test] fn render_real_nan_infinity()    // NaN, Inf rendered as strings
```

### Verification

```bash
./tools/vm.sh test axiomdb-sql 2>&1 | tail -5
```

### Commit

```
feat(fase-22b): add render_fdw_url — URL construction with pushdown params

Step 2 of specs/fase-22b/plan-22b.6-fdw-pushdown.md
```

---

## Step 3 — Update `fdw_scan_table` signature + all call sites

**Goal:** `fdw_scan_table` accepts `pushed_predicates` and `limit`. Reads
`pushdown_cols` / `limit_param` from OPTIONS and calls `render_fdw_url`.
All three call sites updated to compile.

**Files:**
- `crates/axiomdb-sql/src/executor/fdw_http.rs`
- `crates/axiomdb-sql/src/executor/select_joins_ctx.rs` (2 call sites)

**Note:** `select_ctx.rs` call site is updated in Step 4, not here. In this
step leave `select_ctx.rs` passing `&HashMap::new(), None` temporarily so
the workspace compiles cleanly after this step.

### Changes to `fdw_scan_table`

```rust
fn fdw_scan_table(
    storage: &dyn StorageEngine,
    snap: axiomdb_core::TransactionSnapshot,
    table_id: u32,
    columns: &[CatalogColumnDef],
    pushed_predicates: &HashMap<String, Value>,   // NEW
    limit: Option<u64>,                            // NEW
) -> Result<Vec<(RecordId, crate::result::Row)>, DbError> {
    // ... existing reader / options parsing ...

    // NEW: parse pushdown options
    let pushdown_cols_raw = table_opts
        .get("pushdown_cols")
        .cloned()
        .unwrap_or_default();
    let pushdown_cols: Vec<&str> = if pushdown_cols_raw.is_empty() {
        vec![]
    } else {
        pushdown_cols_raw.split(',').map(str::trim).collect()
    };
    let limit_param = table_opts.get("limit_param").map(|s| s.as_str());

    // NEW: build URL with pushdown
    let endpoint = table_opts.get("endpoint").map(|s| s.as_str()).unwrap_or("/");
    let url = render_fdw_url(
        &base_url,
        endpoint,
        pushed_predicates,
        &pushdown_cols,
        limit_param,
        limit,
    );

    let body = http_get(&url, timeout_ms)?;
    json_to_rows(&body, columns)
}
```

The `url` variable previously was `format!("{}{}", base_url..., endpoint)` —
replace that with the call to `render_fdw_url`.

### Changes to `select_joins_ctx.rs` (both call sites)

```rust
// line 15:
fdw_scan_table(storage, snap.clone(), from_t.def.id, &from_t.columns,
               &HashMap::new(), None)?

// line 93:
fdw_scan_table(storage, snap.clone(), jt.def.id, &jt.columns,
               &HashMap::new(), None)?
```

### Changes to `select_ctx.rs` (temporary, replace in Step 4)

```rust
// line 105 — temporary placeholder, Step 4 replaces this
let fdw_rows = fdw_scan_table(storage, snap, resolved.def.id, &resolved.columns,
                               &HashMap::new(), None)?;
```

### Verification

```bash
./tools/vm.sh test axiomdb-sql 2>&1 | tail -10
# All existing FDW tests must still pass.
```

### Commit

```
feat(fase-22b): update fdw_scan_table to accept pushed predicates and limit

Reads pushdown_cols + limit_param from table OPTIONS, calls render_fdw_url.
Join-path callers pass empty map + None (no behavior change).
Step 3 of specs/fase-22b/plan-22b.6-fdw-pushdown.md
```

---

## Step 4 — Wire pushdown into single-table SELECT path

**Goal:** `select_ctx.rs` extracts pushable predicates from WHERE and passes
them to `fdw_scan_table`. Residual WHERE replaces `stmt.where_clause` before
delegating to `execute_select_with_joins_first_materialized`.

**Files:** `crates/axiomdb-sql/src/executor/select_ctx.rs`

### Changes to `select_ctx.rs` lines ~100–116

Replace the current:

```rust
if resolved.def.id >= FOREIGN_TABLE_ID_BASE {
    let fdw_rows = fdw_scan_table(storage, snap, resolved.def.id, &resolved.columns)?;
    let first_source = join_source_schema_from_resolved(&from_table_ref, &resolved);
    let first_rows: Vec<Row> = fdw_rows.into_iter().map(|(_, r)| r).collect();
    return execute_select_with_joins_first_materialized(
        stmt,
        first_source,
        first_rows,
        exec_ctx,
        conn_txn,
        ctx,
    );
}
```

With:

```rust
if resolved.def.id >= FOREIGN_TABLE_ID_BASE {
    // Phase 22b.6: extract equality predicates that can be pushed to the remote.
    let (pushed, residual_where) =
        extract_fdw_pushable(stmt.where_clause.as_ref(), &resolved.columns);

    let pushed_limit: Option<u64> = stmt.limit.as_ref().and_then(|e| match e {
        Expr::Literal(Value::Int(n)) if *n >= 0 => Some(*n as u64),
        Expr::Literal(Value::BigInt(n)) if *n >= 0 => Some(*n as u64),
        _ => None,
    });

    let fdw_rows = fdw_scan_table(
        storage,
        snap,
        resolved.def.id,
        &resolved.columns,
        &pushed,
        pushed_limit,
    )?;

    let first_source = join_source_schema_from_resolved(&from_table_ref, &resolved);
    let first_rows: Vec<Row> = fdw_rows.into_iter().map(|(_, r)| r).collect();

    // Use residual WHERE only — pushed predicates are handled by the remote.
    // LIMIT is intentionally kept in stmt2: the executor applies it locally
    // as well, which is idempotent and guarantees correctness.
    let mut stmt2 = stmt.clone();
    stmt2.where_clause = residual_where;

    return execute_select_with_joins_first_materialized(
        stmt2,
        first_source,
        first_rows,
        exec_ctx,
        conn_txn,
        ctx,
    );
}
```

Make sure `extract_fdw_pushable` is in scope (it lives in `fdw_http.rs` which
is `include!`-d into `mod.rs`, so it should be visible — verify the include
path and add `use` if needed).

### Verification

```bash
./tools/vm.sh test axiomdb-sql 2>&1 | tail -10
# All existing FDW tests must still pass.
```

### Commit

```
feat(fase-22b): wire FDW predicate+limit pushdown into single-table SELECT path

extract_fdw_pushable splits WHERE; residual applied locally.
Step 4 of specs/fase-22b/plan-22b.6-fdw-pushdown.md
```

---

## Step 5 — Integration tests (URL-capturing mock server)

**Goal:** Verify end-to-end that the correct URL is constructed and sent. Uses
a mock HTTP server that records the request line and returns it via a channel.

**Files:** `crates/axiomdb-sql/tests/integration_fdw.rs`

### New helper

```rust
/// Spawns a mock HTTP server that:
/// - Accepts exactly one connection
/// - Sends `captured_tx` the first request line (e.g. "GET /users/5 HTTP/1.1")
/// - Responds with `json_body`
/// Returns the bound port.
fn spawn_capturing_mock_server(
    json_body: &'static str,
    captured_tx: std::sync::mpsc::SyncSender<String>,
) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let first_line = req.lines().next().unwrap_or("").to_string();
            let _ = captured_tx.send(first_line);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                json_body.len(), json_body
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    port
}
```

### Tests to add (≥6)

```rust
#[test]
fn test_pushdown_path_placeholder() {
    // endpoint '/users/{id}', WHERE id=5 → GET /users/5
    // JSON returns rows with id=5 only; assert row returned and URL captured
}

#[test]
fn test_pushdown_query_param_single() {
    // pushdown_cols 'status', WHERE status='active' → GET /items?status=active
}

#[test]
fn test_pushdown_limit_param() {
    // limit_param 'per_page', LIMIT 3 → URL contains ?per_page=3
    // 10 rows in response, only 3 returned (local LIMIT applied)
}

#[test]
fn test_pushdown_mixed_pushed_and_residual() {
    // pushdown_cols 'status', WHERE status='active' AND score > 50
    // → URL has ?status=active; score>50 applied locally
    // Response contains rows with status='active' but varying scores
    // Only rows with score>50 returned
}

#[test]
fn test_pushdown_or_not_pushed() {
    // WHERE status='active' OR id=1 — OR not pushed; URL unchanged (/items)
    // All rows fetched; local filter applied
}

#[test]
fn test_pushdown_no_config_unchanged() {
    // No pushdown_cols, no placeholder, no limit_param
    // WHERE id=5 has no effect on URL
    // URL must be exactly base+endpoint
}

#[test]
fn test_pushdown_path_and_query_no_duplication() {
    // endpoint '/items/{cat}', pushdown_cols 'cat,brand'
    // WHERE cat='shoes' AND brand='nike'
    // → /items/shoes?brand=nike  (cat NOT duplicated as query param)
}

#[test]
fn test_pushdown_unbound_placeholder_left_as_literal() {
    // endpoint '/users/{id}', WHERE name='alice' (no id predicate)
    // URL: /users/{id}?  (placeholder not substituted)
    // All rows from remote filtered locally by name='alice'
}
```

### Verification

```bash
./tools/vm.sh test axiomdb-sql 2>&1 | tail -15
# Must see all new tests green, all existing tests still green.
```

### Commit

```
feat(fase-22b): integration tests for FDW pushdown — URL-capturing mock server

8 new tests verify path placeholders, query params, LIMIT, mixed residual,
OR non-pushdown, no-config baseline, deduplication, unbound placeholder.
Step 5 of specs/fase-22b/plan-22b.6-fdw-pushdown.md
```

---

## Final verification against spec done criteria

- [ ] `extract_fdw_pushable` splits AND-connected equality predicates — Step 1 tests
- [ ] `render_fdw_url` substitutes `{col}` placeholders — Step 2 tests
- [ ] `render_fdw_url` appends `pushdown_cols` params — Step 2 tests
- [ ] `render_fdw_url` appends `limit_param` — Step 2 tests
- [ ] `fdw_scan_table` accepts `pushed_predicates` and `limit` — Step 3
- [ ] Single-table SELECT path extracts and passes pushed predicates — Step 4
- [ ] Residual WHERE preserved and applied locally — Step 5 test_pushdown_mixed
- [ ] LIMIT always applied locally regardless of pushdown — Step 5 test_pushdown_limit_param
- [ ] Join call sites compile with `&HashMap::new(), None` — Step 3
- [ ] All existing FDW tests pass — verified at end of each step
- [ ] ≥6 new integration tests with URL-capturing mock — Step 5 (8 tests)
- [ ] `cargo nextest run -p axiomdb-sql` clean — post Step 5
- [ ] `cargo clippy -p axiomdb-sql -- -D warnings` clean — post Step 5
- [ ] `cargo fmt --check` clean — post Step 5

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| `extract_fdw_pushable` scope issue (function in `fdw_http.rs`, needed in `select_ctx.rs`) | low | Both are in the same `mod executor` via `include!`; verify in Step 4 |
| `stmt.clone()` cost on large projections | negligible | FDW is network-bound; clone of AST is µs |
| Mock server race (test reads captured URL before server sends it) | low | Use `SyncSender` + `recv_timeout(1s)` with panic on timeout |
| `pushdown_cols` option not trimmed — leading/trailing spaces | low | `str::trim()` in split loop (Step 3) |

## Rollback plan

If abandoned mid-way, leave partial work on the current branch. Reset with:
```bash
git reset --hard <commit-sha-before-step-1>
```
Update spec status to `draft` with a note.

## Estimated effort

Total: ~3 hours
- Step 1: 40 min
- Step 2: 45 min
- Step 3: 25 min
- Step 4: 20 min
- Step 5: 50 min
