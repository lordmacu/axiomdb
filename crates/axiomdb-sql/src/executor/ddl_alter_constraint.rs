// ── CHECK constraint enforcement (Phase 4.22b) ────────────────────────────────

/// Evaluates active CHECK constraints for a row about to be inserted/updated.
fn alter_add_constraint(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    table_def: &axiomdb_catalog::ResolvedTable,
    columns_arg: &[axiomdb_catalog::schema::ColumnDef],
    tc: crate::ast::TableConstraint,
    database: &str,
    schema: &str,
) -> Result<(), DbError> {
    use crate::ast::TableConstraint;
    use axiomdb_catalog::schema::ConstraintDef;

    match tc {
        TableConstraint::Unique {
            name,
            columns: col_names,
        } => {
            // ADD CONSTRAINT name UNIQUE (cols) → create a unique index named `name`.
            let idx_name = name.unwrap_or_else(|| {
                format!(
                    "axiom_uq_{}_{}",
                    table_def.def.table_name,
                    col_names.join("_")
                )
            });
            let stmt = crate::ast::CreateIndexStmt {
                name: idx_name,
                table: crate::ast::TableRef {
                    database: None,
                    schema: Some(schema.to_string()),
                    name: table_def.def.table_name.clone(),
                    alias: None,
                },
                columns: col_names
                    .into_iter()
                    .map(|c| crate::ast::IndexColumn {
                        name: c,
                        order: crate::ast::SortOrder::Asc,
                    })
                    .collect(),
                unique: true,
                if_not_exists: false,
                predicate: None,
                fillfactor: None,
                include_columns: vec![],
                index_type: crate::ast::IndexType::BTree,
                pages_per_range: None,
            };
            let mut noop_bloom = crate::bloom::BloomRegistry::new();
            execute_create_index(stmt, storage, txn, conn_txn, &mut noop_bloom, database)?;
            Ok(())
        }

        TableConstraint::Check { name, expr } => {
            let cname = name.ok_or_else(|| DbError::ParseError {
                message: "ADD CONSTRAINT CHECK requires an explicit constraint name".into(),
                position: None,
            })?;

            // Check for duplicate constraint name.
            let snap = txn.active_snapshot(conn_txn);
            {
                let mut reader = CatalogReader::new(storage, snap.clone())?;
                if reader
                    .get_constraint_by_name(table_def.def.id, &cname)?
                    .is_some()
                {
                    return Err(DbError::Other(format!(
                        "constraint '{cname}' already exists on table '{}'",
                        table_def.def.table_name
                    )));
                }
            }

            // Validate all existing rows.
            let existing_rows =
                TableEngine::scan_table(storage, &table_def.def, columns_arg, snap, None)?;
            for (_rid, row_values) in &existing_rows {
                let result = eval(&expr, row_values)?;
                if !crate::eval::is_truthy(&result) {
                    return Err(DbError::CheckViolation {
                        table: table_def.def.table_name.clone(),
                        constraint: cname.clone(),
                    });
                }
            }

            // Serialize the expression to SQL string for persistence.
            let check_expr = expr_to_sql_string(&expr);

            // Persist in axiom_constraints.
            CatalogWriter::new(storage, txn, conn_txn)?.create_constraint(ConstraintDef {
                constraint_id: 0, // allocated by writer
                table_id: table_def.def.id,
                name: cname,
                check_expr,
            })?;
            Ok(())
        }

        TableConstraint::ForeignKey {
            name,
            columns,
            ref_table,
            ref_columns,
            on_delete,
            on_update,
        } => {
            if columns.len() != 1 {
                return Err(DbError::NotImplemented {
                    feature: "composite foreign key (multiple columns) — Phase 6.9".into(),
                });
            }
            let child_col_name = &columns[0];
            let child_col_idx = columns_arg
                .iter()
                .find(|c| &c.name == child_col_name)
                .map(|c| c.col_idx)
                .ok_or_else(|| DbError::ColumnNotFound {
                    name: child_col_name.clone(),
                    table: table_def.def.table_name.clone(),
                })?;
            let ref_col = ref_columns.first().map(|s| s.as_str());

            // Persist the FK definition (validates parent, creates auto-index if needed).
            persist_fk_constraint(
                table_def.def.id,
                &table_def.def.table_name,
                database,
                child_col_idx,
                child_col_name,
                &ref_table,
                ref_col,
                ast_fk_action_to_catalog(on_delete),
                ast_fk_action_to_catalog(on_update),
                name.as_deref(),
                storage,
                txn,
                conn_txn,
            )?;

            // Validate existing data: every non-NULL FK value must reference a parent row.
            let snap = txn.active_snapshot(conn_txn);
            let default_constraint_name = format!(
                "fk_{}_{}_{ref_table}",
                table_def.def.table_name, child_col_name
            );
            let constraint_name = name.as_deref().unwrap_or(&default_constraint_name);
            let new_fk = {
                let mut reader = CatalogReader::new(storage, snap.clone())?;
                reader
                    .get_fk_by_name(table_def.def.id, constraint_name)?
                    .ok_or_else(|| DbError::Internal {
                        message: "FK just created not found in catalog".into(),
                    })?
            };
            let existing_rows =
                TableEngine::scan_table(storage, &table_def.def, columns_arg, snap, None)?;
            let mut noop_bloom = crate::bloom::BloomRegistry::new();
            for (_, row) in &existing_rows {
                if let Err(e) = crate::fk_enforcement::check_fk_child_insert(
                    row,
                    std::slice::from_ref(&new_fk),
                    storage,
                    txn,
                    conn_txn,
                    &mut noop_bloom,
                ) {
                    // Roll back: drop the FK definition (and its auto-created index).
                    let snap2 = txn.active_snapshot(conn_txn);
                    if let Ok(Some(fk)) = CatalogReader::new(storage, snap2)?
                        .get_fk_by_name(table_def.def.id, &new_fk.name)
                    {
                        let fk_index_id = fk.fk_index_id;
                        CatalogWriter::new(storage, txn, conn_txn)?.drop_foreign_key(fk.fk_id)?;
                        if fk_index_id != 0 {
                            let _ = execute_drop_index_by_id(
                                fk_index_id,
                                storage,
                                txn,
                                conn_txn,
                                &mut noop_bloom,
                            );
                        }
                    }
                    return Err(e);
                }
            }

            Ok(())
        }

        TableConstraint::PrimaryKey { .. } => Err(DbError::NotImplemented {
            feature: "ADD CONSTRAINT PRIMARY KEY — requires full table rewrite".into(),
        }),

        TableConstraint::Index { name, columns: col_names } => {
            let idx_name = name.unwrap_or_else(|| {
                format!(
                    "{}_{}_idx",
                    table_def.def.table_name,
                    col_names.join("_")
                )
            });
            let stmt = crate::ast::CreateIndexStmt {
                name: idx_name,
                table: crate::ast::TableRef {
                    database: None,
                    schema: Some(schema.to_string()),
                    name: table_def.def.table_name.clone(),
                    alias: None,
                },
                columns: col_names
                    .into_iter()
                    .map(|c| crate::ast::IndexColumn {
                        name: c,
                        order: crate::ast::SortOrder::Asc,
                    })
                    .collect(),
                unique: false,
                if_not_exists: false,
                predicate: None,
                fillfactor: None,
                include_columns: vec![],
                index_type: crate::ast::IndexType::BTree,
                pages_per_range: None,
            };
            let mut noop_bloom = crate::bloom::BloomRegistry::new();
            execute_create_index(stmt, storage, txn, conn_txn, &mut noop_bloom, database)?;
            Ok(())
        }
    }
}

fn alter_drop_constraint(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    table_def: &axiomdb_catalog::ResolvedTable,
    name: &str,
    if_exists: bool,
) -> Result<(), DbError> {
    let snap = txn.active_snapshot(conn_txn);
    let table_id = table_def.def.id;

    // 1. Search in axiom_indexes (UNIQUE constraints stored as indexes).
    let (idx_id, idx_root) = {
        let mut reader = CatalogReader::new(storage, snap.clone())?;
        let indexes = reader.list_indexes(table_id)?;
        match indexes.into_iter().find(|i| i.name == name) {
            Some(i) => (Some(i.index_id), Some(i.root_page_id)),
            None => (None, None),
        }
    };

    if let Some(index_id) = idx_id {
        CatalogWriter::new(storage, txn, conn_txn)?.delete_index(index_id)?;
        if let Some(root) = idx_root {
            free_btree_pages(storage, root)?;
        }
        return Ok(());
    }

    // 2. Search in axiom_constraints (CHECK constraints).
    let constraint = {
        let mut reader = CatalogReader::new(storage, snap.clone())?;
        reader.get_constraint_by_name(table_id, name)?
    };

    if let Some(c) = constraint {
        CatalogWriter::new(storage, txn, conn_txn)?.drop_constraint(c.constraint_id)?;
        return Ok(());
    }

    // 3. Search in axiom_foreign_keys (FK constraints — Phase 6.5).
    let fk = {
        let mut reader = CatalogReader::new(storage, snap)?;
        reader.get_fk_by_name(table_id, name)?
    };

    if let Some(fk_def) = fk {
        let fk_index_id = fk_def.fk_index_id;
        CatalogWriter::new(storage, txn, conn_txn)?.drop_foreign_key(fk_def.fk_id)?;
        // Drop the auto-created FK index (fk_index_id != 0 means we created it).
        if fk_index_id != 0 {
            let mut noop_bloom = crate::bloom::BloomRegistry::new();
            execute_drop_index_by_id(fk_index_id, storage, txn, conn_txn, &mut noop_bloom)?;
        }
        return Ok(());
    }

    if if_exists {
        Ok(())
    } else {
        Err(DbError::Other(format!(
            "constraint '{name}' not found on table '{}'",
            table_def.def.table_name
        )))
    }
}

/// Converts an [`Expr`] to a SQL string suitable for storing in `axiom_constraints`.
///
/// Not a perfect round-trip — whitespace and casing may differ from the original
/// input, but the output is valid SQL that can be re-parsed and evaluated.
fn expr_to_sql_string(expr: &Expr) -> String {
    use crate::expr::BinaryOp;

    match expr {
        Expr::Literal(v) => match v {
            Value::Int(n) => n.to_string(),
            Value::BigInt(n) => n.to_string(),
            Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
            Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
            Value::Null => "NULL".to_string(),
            Value::Real(f) => f.to_string(),
            _ => format!("{v}"),
        },
        Expr::Column { name, .. } => name.clone(),
        Expr::BinaryOp { left, op, right } => {
            let op_str = match op {
                BinaryOp::Eq => "=",
                BinaryOp::NotEq => "!=",
                BinaryOp::Lt => "<",
                BinaryOp::LtEq => "<=",
                BinaryOp::Gt => ">",
                BinaryOp::GtEq => ">=",
                BinaryOp::And => "AND",
                BinaryOp::Or => "OR",
                BinaryOp::Add => "+",
                BinaryOp::Sub => "-",
                BinaryOp::Mul => "*",
                BinaryOp::Div => "/",
                BinaryOp::Mod => "%",
                BinaryOp::Concat => "||",
                BinaryOp::Xor => "XOR",
                BinaryOp::NullSafe => "<=>",
                BinaryOp::IntDiv => "DIV",
                BinaryOp::BitAnd => "&",
                BinaryOp::BitOr => "|",
                BinaryOp::BitXor => "^",
                BinaryOp::ShiftLeft => "<<",
                BinaryOp::ShiftRight => ">>",
                BinaryOp::Regexp => "REGEXP",
            };
            format!(
                "({} {op_str} {})",
                expr_to_sql_string(left),
                expr_to_sql_string(right)
            )
        }
        Expr::UnaryOp {
            op: crate::expr::UnaryOp::Not,
            operand,
        } => {
            format!("NOT {}", expr_to_sql_string(operand))
        }
        Expr::IsNull {
            expr: inner,
            negated: false,
        } => {
            format!("{} IS NULL", expr_to_sql_string(inner))
        }
        Expr::IsNull {
            expr: inner,
            negated: true,
        } => {
            format!("{} IS NOT NULL", expr_to_sql_string(inner))
        }
        // For complex expressions not yet handled, fall back to a debug representation.
        other => format!("{other:?}"),
    }
}
