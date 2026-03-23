//! Integration tests for the DML parser (subfase 4.4).

use nexusdb_sql::{
    ast::{
        FromClause, InsertSource, JoinCondition, JoinType, NullsOrder, SelectItem, SortOrder, Stmt,
    },
    expr::{BinaryOp, Expr},
    parse,
};
use nexusdb_types::Value;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn select(sql: &str) -> nexusdb_sql::ast::SelectStmt {
    match parse(sql, None).unwrap() {
        Stmt::Select(s) => s,
        other => panic!("expected Select, got {other:?}"),
    }
}

fn parse_err(sql: &str) -> nexusdb_core::DbError {
    parse(sql, None).unwrap_err()
}

// ── Expression extensions ──────────────────────────────────────────────────────

#[test]
fn test_arithmetic_add() {
    let s = select("SELECT 1 + 2");
    assert!(matches!(
        &s.columns[0],
        SelectItem::Expr {
            expr: Expr::BinaryOp {
                op: BinaryOp::Add,
                ..
            },
            ..
        }
    ));
}

#[test]
fn test_arithmetic_precedence() {
    // 2 + 3 * 4 should be 2 + (3*4), not (2+3)*4
    let s = select("SELECT 2 + 3 * 4");
    // Outermost is Add
    assert!(matches!(
        &s.columns[0],
        SelectItem::Expr {
            expr: Expr::BinaryOp {
                op: BinaryOp::Add,
                ..
            },
            ..
        }
    ));
}

#[test]
fn test_concat_operator() {
    let s = select("SELECT 'a' || 'b'");
    assert!(matches!(
        &s.columns[0],
        SelectItem::Expr {
            expr: Expr::BinaryOp {
                op: BinaryOp::Concat,
                ..
            },
            ..
        }
    ));
}

#[test]
fn test_is_null() {
    let s = select("SELECT * FROM t WHERE email IS NULL");
    assert!(matches!(
        &s.where_clause,
        Some(Expr::IsNull { negated: false, .. })
    ));
}

#[test]
fn test_is_not_null() {
    let s = select("SELECT * FROM t WHERE email IS NOT NULL");
    assert!(matches!(
        &s.where_clause,
        Some(Expr::IsNull { negated: true, .. })
    ));
}

#[test]
fn test_between() {
    let s = select("SELECT * FROM t WHERE age BETWEEN 18 AND 65");
    assert!(matches!(
        &s.where_clause,
        Some(Expr::Between { negated: false, .. })
    ));
}

#[test]
fn test_not_between() {
    let s = select("SELECT * FROM t WHERE age NOT BETWEEN 18 AND 65");
    assert!(matches!(
        &s.where_clause,
        Some(Expr::Between { negated: true, .. })
    ));
}

#[test]
fn test_like() {
    let s = select("SELECT * FROM t WHERE name LIKE 'A%'");
    assert!(matches!(
        &s.where_clause,
        Some(Expr::Like { negated: false, .. })
    ));
}

#[test]
fn test_not_like() {
    let s = select("SELECT * FROM t WHERE name NOT LIKE 'A%'");
    assert!(matches!(
        &s.where_clause,
        Some(Expr::Like { negated: true, .. })
    ));
}

#[test]
fn test_in_list() {
    let s = select("SELECT * FROM t WHERE id IN (1, 2, 3)");
    assert!(matches!(
        &s.where_clause,
        Some(Expr::In { negated: false, list, .. }) if list.len() == 3
    ));
}

#[test]
fn test_not_in_list() {
    let s = select("SELECT * FROM t WHERE id NOT IN (1, 2)");
    assert!(matches!(
        &s.where_clause,
        Some(Expr::In { negated: true, .. })
    ));
}

#[test]
fn test_function_call() {
    let s = select("SELECT abs(price) FROM t");
    assert!(matches!(
        &s.columns[0],
        SelectItem::Expr {
            expr: Expr::Function { name, args },
            ..
        } if name == "abs" && args.len() == 1
    ));
}

#[test]
fn test_count_star() {
    let s = select("SELECT COUNT(*) FROM t");
    assert!(matches!(
        &s.columns[0],
        SelectItem::Expr {
            expr: Expr::Function { name, args },
            ..
        } if name == "count" && args.is_empty()
    ));
}

#[test]
fn test_table_dot_column() {
    let s = select("SELECT u.id FROM users u");
    assert!(matches!(
        &s.columns[0],
        SelectItem::Expr {
            expr: Expr::Column { name, .. },
            ..
        } if name == "u.id"
    ));
}

#[test]
fn test_nested_arithmetic_with_comparison() {
    // price * 1.1 > 100
    let s = select("SELECT * FROM t WHERE price * 1.1 > 100");
    assert!(matches!(
        &s.where_clause,
        Some(Expr::BinaryOp {
            op: BinaryOp::Gt,
            ..
        })
    ));
}

// ── SELECT without FROM ───────────────────────────────────────────────────────

#[test]
fn test_select_without_from() {
    let s = select("SELECT 1");
    assert!(s.from.is_none());
    assert!(s.joins.is_empty());
    assert_eq!(s.columns.len(), 1);
}

#[test]
fn test_select_multiple_literals_no_from() {
    let s = select("SELECT 1, 'hello', TRUE");
    assert_eq!(s.columns.len(), 3);
    assert!(s.from.is_none());
}

// ── SELECT list ───────────────────────────────────────────────────────────────

#[test]
fn test_select_wildcard() {
    let s = select("SELECT * FROM t");
    assert!(matches!(s.columns[0], SelectItem::Wildcard));
}

#[test]
fn test_select_qualified_wildcard() {
    let s = select("SELECT users.* FROM users");
    assert!(matches!(
        &s.columns[0],
        SelectItem::QualifiedWildcard(name) if name == "users"
    ));
}

#[test]
fn test_select_column_with_alias() {
    let s = select("SELECT id AS user_id FROM users");
    assert!(matches!(
        &s.columns[0],
        SelectItem::Expr { alias: Some(a), .. } if a == "user_id"
    ));
}

#[test]
fn test_select_distinct() {
    let s = select("SELECT DISTINCT country FROM users");
    assert!(s.distinct);
}

#[test]
fn test_select_not_distinct_by_default() {
    let s = select("SELECT id FROM t");
    assert!(!s.distinct);
}

// ── FROM clause ───────────────────────────────────────────────────────────────

#[test]
fn test_select_from_table() {
    let s = select("SELECT * FROM users");
    assert!(matches!(
        &s.from,
        Some(FromClause::Table(tr)) if tr.name == "users"
    ));
}

#[test]
fn test_select_from_table_with_as_alias() {
    let s = select("SELECT * FROM users AS u");
    if let Some(FromClause::Table(tr)) = &s.from {
        assert_eq!(tr.name, "users");
        assert_eq!(tr.alias.as_deref(), Some("u"));
    } else {
        panic!("expected Table from clause");
    }
}

#[test]
fn test_select_from_table_implicit_alias() {
    let s = select("SELECT * FROM users u");
    if let Some(FromClause::Table(tr)) = &s.from {
        assert_eq!(tr.alias.as_deref(), Some("u"));
    } else {
        panic!("expected Table from clause");
    }
}

#[test]
fn test_select_from_subquery() {
    let s = select("SELECT id FROM (SELECT id FROM users WHERE active = TRUE) AS sub");
    assert!(matches!(&s.from, Some(FromClause::Subquery { alias, .. }) if alias == "sub"));
}

// ── JOIN ──────────────────────────────────────────────────────────────────────

#[test]
fn test_select_inner_join() {
    let s = select("SELECT * FROM users u INNER JOIN orders o ON u.id = o.user_id");
    assert_eq!(s.joins.len(), 1);
    assert_eq!(s.joins[0].join_type, JoinType::Inner);
    assert!(matches!(s.joins[0].condition, JoinCondition::On(_)));
}

#[test]
fn test_select_bare_join_is_inner() {
    let s = select("SELECT * FROM a JOIN b ON a.id = b.aid");
    assert_eq!(s.joins[0].join_type, JoinType::Inner);
}

#[test]
fn test_select_left_join() {
    let s = select("SELECT * FROM a LEFT JOIN b ON a.id = b.id");
    assert_eq!(s.joins[0].join_type, JoinType::Left);
}

#[test]
fn test_select_left_outer_join() {
    // OUTER is optional
    let s = select("SELECT * FROM a LEFT OUTER JOIN b ON a.id = b.id");
    assert_eq!(s.joins[0].join_type, JoinType::Left);
}

#[test]
fn test_select_right_join() {
    let s = select("SELECT * FROM a RIGHT JOIN b ON a.id = b.id");
    assert_eq!(s.joins[0].join_type, JoinType::Right);
}

#[test]
fn test_select_cross_join() {
    let s = select("SELECT * FROM a CROSS JOIN b");
    assert_eq!(s.joins[0].join_type, JoinType::Cross);
}

#[test]
fn test_select_join_using() {
    let s = select("SELECT * FROM users JOIN orders USING (user_id)");
    assert!(matches!(
        &s.joins[0].condition,
        JoinCondition::Using(cols) if cols == &["user_id"]
    ));
}

#[test]
fn test_select_multiple_joins() {
    let s = select("SELECT * FROM a JOIN b ON a.id = b.id JOIN c ON b.id = c.id");
    assert_eq!(s.joins.len(), 2);
}

// ── WHERE ─────────────────────────────────────────────────────────────────────

#[test]
fn test_select_where_eq() {
    let s = select("SELECT * FROM t WHERE id = 1");
    assert!(matches!(
        &s.where_clause,
        Some(Expr::BinaryOp {
            op: BinaryOp::Eq,
            ..
        })
    ));
}

#[test]
fn test_select_where_and() {
    let s = select("SELECT * FROM t WHERE a = 1 AND b = 2");
    assert!(matches!(
        &s.where_clause,
        Some(Expr::BinaryOp {
            op: BinaryOp::And,
            ..
        })
    ));
}

#[test]
fn test_select_where_or() {
    let s = select("SELECT * FROM t WHERE a = 1 OR b = 2");
    assert!(matches!(
        &s.where_clause,
        Some(Expr::BinaryOp {
            op: BinaryOp::Or,
            ..
        })
    ));
}

// ── GROUP BY + HAVING ─────────────────────────────────────────────────────────

#[test]
fn test_select_group_by() {
    let s = select("SELECT country FROM users GROUP BY country");
    assert_eq!(s.group_by.len(), 1);
}

#[test]
fn test_select_group_by_multiple() {
    let s = select("SELECT a, b FROM t GROUP BY a, b");
    assert_eq!(s.group_by.len(), 2);
}

#[test]
fn test_select_having() {
    let s = select("SELECT country, COUNT(*) FROM users GROUP BY country HAVING COUNT(*) > 10");
    assert!(s.having.is_some());
}

// ── ORDER BY ─────────────────────────────────────────────────────────────────

#[test]
fn test_select_order_by_asc() {
    let s = select("SELECT * FROM t ORDER BY name ASC");
    assert_eq!(s.order_by[0].order, SortOrder::Asc);
}

#[test]
fn test_select_order_by_desc() {
    let s = select("SELECT * FROM t ORDER BY price DESC");
    assert_eq!(s.order_by[0].order, SortOrder::Desc);
}

#[test]
fn test_select_order_by_default_asc() {
    let s = select("SELECT * FROM t ORDER BY name");
    assert_eq!(s.order_by[0].order, SortOrder::Asc);
}

#[test]
fn test_select_order_by_nulls_first() {
    let s = select("SELECT * FROM t ORDER BY price NULLS FIRST");
    assert_eq!(s.order_by[0].nulls, Some(NullsOrder::First));
}

#[test]
fn test_select_order_by_nulls_last() {
    let s = select("SELECT * FROM t ORDER BY price DESC NULLS LAST");
    assert_eq!(s.order_by[0].order, SortOrder::Desc);
    assert_eq!(s.order_by[0].nulls, Some(NullsOrder::Last));
}

#[test]
fn test_select_order_by_multiple() {
    let s = select("SELECT * FROM t ORDER BY a ASC, b DESC");
    assert_eq!(s.order_by.len(), 2);
}

// ── LIMIT / OFFSET ────────────────────────────────────────────────────────────

#[test]
fn test_select_limit() {
    let s = select("SELECT * FROM t LIMIT 10");
    assert!(matches!(s.limit, Some(Expr::Literal(Value::Int(10)))));
    assert!(s.offset.is_none());
}

#[test]
fn test_select_limit_offset() {
    let s = select("SELECT * FROM t LIMIT 20 OFFSET 40");
    assert!(matches!(s.limit, Some(Expr::Literal(Value::Int(20)))));
    assert!(matches!(s.offset, Some(Expr::Literal(Value::Int(40)))));
}

// ── Full SELECT query ─────────────────────────────────────────────────────────

#[test]
fn test_select_full_query() {
    let sql = "
        SELECT DISTINCT u.id AS user_id, u.name, COUNT(o.id) AS order_count
        FROM users AS u
        LEFT JOIN orders AS o ON u.id = o.user_id
        WHERE u.active = TRUE AND u.age >= 18
        GROUP BY u.id, u.name
        HAVING COUNT(o.id) > 0
        ORDER BY order_count DESC NULLS LAST
        LIMIT 50 OFFSET 0
    ";
    let s = select(sql);
    assert!(s.distinct);
    assert_eq!(s.columns.len(), 3);
    assert!(s.from.is_some());
    assert_eq!(s.joins.len(), 1);
    assert_eq!(s.joins[0].join_type, JoinType::Left);
    assert!(s.where_clause.is_some());
    assert_eq!(s.group_by.len(), 2);
    assert!(s.having.is_some());
    assert_eq!(s.order_by.len(), 1);
    assert_eq!(s.order_by[0].order, SortOrder::Desc);
    assert_eq!(s.order_by[0].nulls, Some(NullsOrder::Last));
    assert!(s.limit.is_some());
    assert!(s.offset.is_some());
}

// ── INSERT ────────────────────────────────────────────────────────────────────

#[test]
fn test_insert_values_single_row() {
    match parse("INSERT INTO users (id, name) VALUES (1, 'Alice')", None).unwrap() {
        Stmt::Insert(ins) => {
            assert_eq!(ins.table.name, "users");
            assert_eq!(ins.columns.as_ref().unwrap(), &["id", "name"]);
            if let InsertSource::Values(rows) = &ins.source {
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].len(), 2);
            } else {
                panic!("expected Values source");
            }
        }
        other => panic!("expected Insert, got {other:?}"),
    }
}

#[test]
fn test_insert_values_multi_row() {
    match parse("INSERT INTO t VALUES (1), (2), (3)", None).unwrap() {
        Stmt::Insert(ins) => {
            if let InsertSource::Values(rows) = &ins.source {
                assert_eq!(rows.len(), 3);
            } else {
                panic!("expected Values source");
            }
        }
        other => panic!("expected Insert, got {other:?}"),
    }
}

#[test]
fn test_insert_without_column_list() {
    match parse("INSERT INTO t VALUES (1, 'x')", None).unwrap() {
        Stmt::Insert(ins) => {
            assert!(ins.columns.is_none());
        }
        other => panic!("expected Insert, got {other:?}"),
    }
}

#[test]
fn test_insert_default_values() {
    match parse("INSERT INTO t DEFAULT VALUES", None).unwrap() {
        Stmt::Insert(ins) => {
            assert!(matches!(ins.source, InsertSource::DefaultValues));
        }
        other => panic!("expected Insert, got {other:?}"),
    }
}

#[test]
fn test_insert_select() {
    match parse("INSERT INTO t2 SELECT * FROM t1 WHERE active = TRUE", None).unwrap() {
        Stmt::Insert(ins) => {
            assert!(matches!(ins.source, InsertSource::Select(_)));
        }
        other => panic!("expected Insert, got {other:?}"),
    }
}

// ── UPDATE ────────────────────────────────────────────────────────────────────

#[test]
fn test_update_single_col() {
    match parse("UPDATE users SET name = 'Alice' WHERE id = 1", None).unwrap() {
        Stmt::Update(upd) => {
            assert_eq!(upd.table.name, "users");
            assert_eq!(upd.assignments.len(), 1);
            assert_eq!(upd.assignments[0].column, "name");
            assert!(upd.where_clause.is_some());
        }
        other => panic!("expected Update, got {other:?}"),
    }
}

#[test]
fn test_update_multi_col() {
    match parse("UPDATE t SET a = 1, b = 2, c = 3 WHERE id = 99", None).unwrap() {
        Stmt::Update(upd) => {
            assert_eq!(upd.assignments.len(), 3);
        }
        other => panic!("expected Update, got {other:?}"),
    }
}

#[test]
fn test_update_without_where() {
    match parse("UPDATE t SET col = 0", None).unwrap() {
        Stmt::Update(upd) => {
            assert!(upd.where_clause.is_none());
        }
        other => panic!("expected Update, got {other:?}"),
    }
}

// ── DELETE ────────────────────────────────────────────────────────────────────

#[test]
fn test_delete_with_where() {
    match parse("DELETE FROM users WHERE id = 42", None).unwrap() {
        Stmt::Delete(del) => {
            assert_eq!(del.table.name, "users");
            assert!(del.where_clause.is_some());
        }
        other => panic!("expected Delete, got {other:?}"),
    }
}

#[test]
fn test_delete_without_where() {
    match parse("DELETE FROM t", None).unwrap() {
        Stmt::Delete(del) => {
            assert!(del.where_clause.is_none());
        }
        other => panic!("expected Delete, got {other:?}"),
    }
}

// ── Error cases ───────────────────────────────────────────────────────────────

#[test]
fn test_error_select_missing_from_table() {
    let e = parse_err("SELECT * FROM");
    assert!(matches!(e, nexusdb_core::DbError::ParseError { .. }));
}

#[test]
fn test_error_insert_missing_values() {
    let e = parse_err("INSERT INTO t (id) GARBAGE (1)");
    assert!(matches!(e, nexusdb_core::DbError::ParseError { .. }));
}

#[test]
fn test_error_update_missing_set() {
    let e = parse_err("UPDATE t WHERE id = 1");
    assert!(matches!(e, nexusdb_core::DbError::ParseError { .. }));
}

#[test]
fn test_error_delete_missing_from() {
    let e = parse_err("DELETE users WHERE id = 1");
    assert!(matches!(e, nexusdb_core::DbError::ParseError { .. }));
}
