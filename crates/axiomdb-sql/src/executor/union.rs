// ── UNION / INTERSECT / EXCEPT executor ──────────────────────────────────────

/// Executes a set-operation chain: `first` followed by a sequence of tails
/// applied left-to-right. Each tail has its own kind (UNION/INTERSECT/EXCEPT)
/// and ALL flag.
///
/// Semantics (per PostgreSQL / SQL std):
/// - UNION     — distinct concatenation (hash dedup)
/// - UNION ALL — concatenation (keeps duplicates)
/// - INTERSECT — rows present in both sides, distinct
/// - INTERSECT ALL — per group, output min(L,R) copies
/// - EXCEPT    — rows in left but not in right, distinct
/// - EXCEPT ALL — per group, output max(0, L-R) copies
///
/// Column metadata comes from the first SELECT. Every tail must produce
/// the same arity.
/// Public entry point for set operations embedded inside subqueries (Phase 21.9b).
pub(crate) fn execute_set_op_ctx(
    first: SelectStmt,
    rest: Vec<SetOpTail>,
    exec_ctx: &ExecutionContext,
    conn_txn: Option<&ConnectionTxn>,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    execute_set_op(first, rest, exec_ctx, conn_txn, ctx)
}

fn execute_set_op(
    first: SelectStmt,
    rest: Vec<SetOpTail>,
    exec_ctx: &ExecutionContext,
    conn_txn: Option<&ConnectionTxn>,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let first_result = execute_select_ctx(first, exec_ctx, conn_txn, ctx)?;
    let (columns, mut acc_rows) = match first_result {
        QueryResult::Rows { columns, rows } => (columns, rows),
        other => return Ok(other),
    };
    let width = columns.len();

    for tail in rest {
        let right_result = execute_select_ctx(tail.select, exec_ctx, conn_txn, ctx)?;
        let right_rows = match right_result {
            QueryResult::Rows { columns: cols, rows } => {
                if cols.len() != width {
                    return Err(DbError::ParseError {
                        message: format!(
                            "set operation branches must have the same number of columns \
                             (first has {width}, this one has {})",
                            cols.len()
                        ),
                        position: None,
                    });
                }
                rows
            }
            other => return Ok(other),
        };

        acc_rows = match tail.kind {
            SetOpKind::Union => apply_union(acc_rows, right_rows, tail.all),
            SetOpKind::Intersect => apply_intersect(acc_rows, right_rows, tail.all),
            SetOpKind::Except => apply_except(acc_rows, right_rows, tail.all),
        };
    }

    Ok(QueryResult::Rows { columns, rows: acc_rows })
}

fn apply_union(mut left: Vec<Row>, right: Vec<Row>, all: bool) -> Vec<Row> {
    left.extend(right);
    if all {
        return left;
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(left.len());
    for row in left {
        let key = row_to_dedup_key(&row);
        if seen.insert(key) {
            out.push(row);
        }
    }
    out
}

fn apply_intersect(left: Vec<Row>, right: Vec<Row>, all: bool) -> Vec<Row> {
    // Build per-group counts for the right side.
    let mut right_counts: std::collections::HashMap<Vec<u8>, usize> =
        std::collections::HashMap::new();
    for row in &right {
        *right_counts.entry(row_to_dedup_key(row)).or_insert(0) += 1;
    }

    if all {
        // INTERSECT ALL: per row in left, emit if right still has it; decrement.
        let mut out = Vec::new();
        for row in left {
            let key = row_to_dedup_key(&row);
            if let Some(cnt) = right_counts.get_mut(&key) {
                if *cnt > 0 {
                    *cnt -= 1;
                    out.push(row);
                }
            }
        }
        out
    } else {
        // INTERSECT: emit each group once, iff both sides contain it.
        let mut emitted = std::collections::HashSet::new();
        let mut out = Vec::new();
        for row in left {
            let key = row_to_dedup_key(&row);
            if right_counts.contains_key(&key) && emitted.insert(key) {
                out.push(row);
            }
        }
        out
    }
}

fn apply_except(left: Vec<Row>, right: Vec<Row>, all: bool) -> Vec<Row> {
    // Counts of right-side groups to subtract from left.
    let mut right_counts: std::collections::HashMap<Vec<u8>, usize> =
        std::collections::HashMap::new();
    for row in &right {
        *right_counts.entry(row_to_dedup_key(row)).or_insert(0) += 1;
    }

    if all {
        // EXCEPT ALL: for each left row, skip if right still has a matching copy.
        let mut out = Vec::new();
        for row in left {
            let key = row_to_dedup_key(&row);
            if let Some(cnt) = right_counts.get_mut(&key) {
                if *cnt > 0 {
                    *cnt -= 1;
                    continue;
                }
            }
            out.push(row);
        }
        out
    } else {
        // EXCEPT: each distinct left group, emit once iff not in right.
        let mut emitted = std::collections::HashSet::new();
        let mut out = Vec::new();
        for row in left {
            let key = row_to_dedup_key(&row);
            if !right_counts.contains_key(&key) && emitted.insert(key) {
                out.push(row);
            }
        }
        out
    }
}

/// Creates a deduplication key from a row by serializing all values.
///
/// Uses a simple but correct serialization: each value is converted to its
/// typed byte representation with a type tag so `Int(0)` ≠ `BigInt(0)` ≠
/// `Text("0")`. NULLs are encoded explicitly.
fn row_to_dedup_key(row: &Row) -> Vec<u8> {
    let mut key = Vec::new();
    for value in row.iter() {
        match value {
            Value::Null => key.push(0),
            Value::Bool(b) => {
                key.push(1);
                key.push(*b as u8);
            }
            Value::Int(n) => {
                key.push(2);
                key.extend_from_slice(&n.to_le_bytes());
            }
            Value::BigInt(n) => {
                key.push(3);
                key.extend_from_slice(&n.to_le_bytes());
            }
            Value::Real(f) => {
                key.push(4);
                key.extend_from_slice(&f.to_bits().to_le_bytes());
            }
            Value::Decimal(m, s) => {
                key.push(5);
                key.extend_from_slice(&m.to_le_bytes());
                key.push(*s);
            }
            Value::Date(d) => {
                key.push(6);
                key.extend_from_slice(&d.to_le_bytes());
            }
            Value::Timestamp(t) => {
                key.push(7);
                key.extend_from_slice(&t.to_le_bytes());
            }
            Value::Text(s) | Value::Json(s) => {
                key.push(8);
                key.extend_from_slice(s.as_bytes());
                key.push(0);
            }
            Value::Bytes(b) => {
                key.push(9);
                key.extend_from_slice(b);
                key.push(0);
            }
            Value::Uuid(u) => {
                key.push(10);
                key.extend_from_slice(u);
            }
            Value::Jsonb(b) => {
                key.push(11);
                key.extend_from_slice(b);
                key.push(0);
            }
            Value::Array(_elems) => {
                // Array dedup key deferred to Step 5 (operators).
                key.push(13);
                key.push(0);
            }
            Value::Range(rv) => {
                key.push(14);
                let s = rv.to_display_string();
                key.extend_from_slice(s.as_bytes());
                key.push(0);
            }
        }
    }
    key
}
