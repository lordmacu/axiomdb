// ── SQL cursors ───────────────────────────────────────────────────────────────

fn execute_declare_cursor(
    stmt: crate::ast::DeclareCursorStmt,
    exec_ctx: &ExecutionContext,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    if !ctx.in_explicit_txn || ctx.conn_txn.is_none() {
        return Err(DbError::InvalidValue {
            reason: "DECLARE CURSOR requires an active explicit transaction".into(),
        });
    }

    let result = match *stmt.query {
        Stmt::Select(s) => {
            let conn = ctx.conn_txn.take().expect("conn_txn set");
            let r = execute_select_ctx(s, exec_ctx, Some(&conn), ctx);
            ctx.conn_txn = Some(conn);
            r?
        }
        Stmt::SetOp { first, rest } => {
            let conn = ctx.conn_txn.take().expect("conn_txn set");
            let r = execute_set_op(first, rest, exec_ctx, Some(&conn), ctx);
            ctx.conn_txn = Some(conn);
            r?
        }
        _ => {
            return Err(DbError::InvalidValue {
                reason: "DECLARE CURSOR requires a row-returning query".into(),
            })
        }
    };

    let (columns, rows) = match result {
        QueryResult::Rows { columns, rows } => (columns, rows),
        _ => {
            return Err(DbError::InvalidValue {
                reason: "DECLARE CURSOR requires a row-returning query".into(),
            })
        }
    };

    ctx.declare_cursor(
        &stmt.name,
        crate::session::SessionCursor {
            columns,
            rows,
            pos: 0,
        },
    )?;
    Ok(QueryResult::Empty)
}

fn execute_fetch_cursor(
    stmt: crate::ast::FetchCursorStmt,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    if !ctx.in_explicit_txn || ctx.conn_txn.is_none() {
        return Err(DbError::InvalidValue {
            reason: "FETCH requires an active explicit transaction".into(),
        });
    }

    let cursor = ctx.cursor_mut(&stmt.name).ok_or_else(|| DbError::InvalidValue {
        reason: format!("cursor '{}' was not found", stmt.name),
    })?;
    let start = cursor.pos;
    let end = match stmt.count {
        crate::ast::FetchCount::Next => start.saturating_add(1).min(cursor.rows.len()),
        crate::ast::FetchCount::Forward(n) => {
            start.saturating_add(n.try_into().unwrap_or(usize::MAX)).min(cursor.rows.len())
        }
        crate::ast::FetchCount::All => cursor.rows.len(),
    };
    let columns = cursor.columns.clone();
    let rows = cursor.rows[start..end].to_vec();
    cursor.pos = end;
    if rows.is_empty() {
        Ok(QueryResult::empty_rows(columns))
    } else {
        Ok(QueryResult::Rows { columns, rows })
    }
}

fn execute_close_cursor(
    stmt: crate::ast::CloseCursorStmt,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    match stmt {
        crate::ast::CloseCursorStmt::One(name) => ctx.close_cursor(&name)?,
        crate::ast::CloseCursorStmt::All => ctx.close_all_cursors(),
    }
    Ok(QueryResult::Empty)
}
