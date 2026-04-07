// ── Prepared statement helpers ────────────────────────────────────────────────

/// Extracts the result column metadata from an analyzed SELECT statement.
/// Returns an empty vec for non-SELECT statements (INSERT/UPDATE/DELETE/DDL).
fn extract_result_columns(stmt: &Stmt) -> Vec<ColumnMeta> {
    use axiomdb_sql::ast::SelectItem;
    match stmt {
        Stmt::Select(s) => s
            .columns
            .iter()
            .map(|item| match item {
                SelectItem::Expr { alias, expr } => {
                    let name = alias.clone().unwrap_or_else(|| format!("{expr:?}"));
                    ColumnMeta::computed(name, DataType::Text) // type unknown without full inference
                }
                SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                    ColumnMeta::computed("*".to_string(), DataType::Text)
                }
            })
            .collect(),
        _ => vec![],
    }
}

// ── Status counter helpers ─────────────────────────────────────────────────────

/// Increments Questions, Com_select, and Com_insert for one processed statement.
fn bump_statement_counters(
    status: &Arc<StatusRegistry>,
    sess: &mut super::status::SessionStatus,
    class: SqlCommandClass,
) {
    status.questions.fetch_add(1, Ordering::Relaxed);
    sess.questions += 1;
    match class {
        SqlCommandClass::Select => {
            status.com_select.fetch_add(1, Ordering::Relaxed);
            sess.com_select += 1;
        }
        SqlCommandClass::Insert => {
            status.com_insert.fetch_add(1, Ordering::Relaxed);
            sess.com_insert += 1;
        }
        SqlCommandClass::Other => {}
    }
}

/// Increments Bytes_sent by `nbytes` in both the global registry and the
/// per-connection session counters.
fn bump_bytes_sent(
    nbytes: u64,
    status: &Arc<StatusRegistry>,
    sess: &mut super::status::SessionStatus,
) {
    status.bytes_sent.fetch_add(nbytes, Ordering::Relaxed);
    sess.bytes_sent += nbytes;
}

/// Total wire size of a packet batch (payload + 4-byte MySQL header per packet).
fn wire_size(packets: &[(u8, Vec<u8>)]) -> u64 {
    packets.iter().map(|(_, p)| p.len() as u64 + 4).sum()
}

#[cfg(test)]
mod tests {
    use super::{show_variables_result, ConnectionState};

    #[test]
    fn test_show_variables_includes_on_error() {
        let conn = ConnectionState::new();
        let packets = show_variables_result("show variables like 'on_error'", &conn);
        let payloads: Vec<u8> = packets.into_iter().flat_map(|(_, p)| p).collect();
        assert!(
            payloads.windows("on_error".len()).any(|w| w == b"on_error"),
            "SHOW VARIABLES LIKE 'on_error' must include the variable name"
        );
        assert!(
            payloads
                .windows("rollback_statement".len())
                .any(|w| w == b"rollback_statement"),
            "SHOW VARIABLES LIKE 'on_error' must expose the live default value"
        );
    }
}
