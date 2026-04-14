// ── INSERT ────────────────────────────────────────────────────────────────────

fn analyze_insert(
    mut s: InsertStmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
) -> Result<InsertStmt, DbError> {
    let mut reader = CatalogReader::new(storage, snapshot.clone())?;
    let table_def = resolve_dml_table(&mut reader, &s.table, default_database, default_schema)?;
    let columns = reader.list_columns(table_def.id)?;

    // Validate named column list if provided.
    if let Some(ref col_names) = s.columns {
        for col_name in col_names {
            if !columns.iter().any(|c| &c.name == col_name) {
                let available = columns
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(DbError::ColumnNotFound {
                    name: col_name.clone(),
                    table: format!("\"{}\" (available: {})", s.table.name, available),
                });
            }
        }
    }

    // Analyze SELECT source if present.
    if let InsertSource::Select(ref select) = s.source {
        let analyzed = analyze_select(
            *select.clone(),
            storage,
            snapshot.clone(),
            default_database,
            default_schema,
        )?;
        s.source = InsertSource::Select(Box::new(analyzed));
    }

    // Phase 21.4 — resolve RETURNING projection against the target table's scope.
    if !s.returning.is_empty() {
        let mut reader = CatalogReader::new(storage, snapshot.clone())?;
        let table_def =
            resolve_dml_table(&mut reader, &s.table, default_database, default_schema)?;
        let columns = reader.list_columns(table_def.id)?;
        let bound = BoundTable {
            alias: s.table.alias.clone(),
            name: s.table.name.clone(),
            columns,
            col_offset: 0,
        };
        let ctx = BindContext { tables: vec![bound] };
        let mut resolved = Vec::with_capacity(s.returning.len());
        for item in std::mem::take(&mut s.returning) {
            resolved.push(match item {
                SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => item,
                SelectItem::Expr { expr, alias } => SelectItem::Expr {
                    expr: resolve_expr(expr, &ctx)?,
                    alias,
                },
            });
        }
        s.returning = resolved;
    }

    Ok(s)
}

// ── UPDATE ────────────────────────────────────────────────────────────────────

fn analyze_update(
    mut s: UpdateStmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
) -> Result<UpdateStmt, DbError> {
    let mut reader = CatalogReader::new(storage, snapshot.clone())?;
    let table_def = resolve_dml_table(&mut reader, &s.table, default_database, default_schema)?;
    let columns = reader.list_columns(table_def.id)?;

    let ctx = if s.joins.is_empty() {
        let bound = BoundTable {
            alias: s.table.alias.clone(),
            name: s.table.name.clone(),
            columns: columns.clone(),
            col_offset: 0,
        };
        BindContext {
            tables: vec![bound],
        }
    } else {
        build_context(
            &Some(FromClause::Table(s.table.clone())),
            &s.joins,
            storage,
            snapshot.clone(),
            default_database,
            default_schema,
        )?
    };

    let mut resolved_joins = Vec::with_capacity(s.joins.len());
    for mut join in s.joins {
        // Phase 11.20d4: resolve JSON_TABLE doc + PASSING against the combined
        // UPDATE scope so correlated doc / PASSING bind to the target table's
        // columns.
        if let FromClause::JsonTable(jt) = &mut join.table {
            let taken_doc = std::mem::replace(
                &mut jt.doc,
                Expr::Literal(axiomdb_types::Value::Null),
            );
            jt.doc = resolve_expr(taken_doc, &ctx)?;
            for (expr, _name) in &mut jt.passing {
                let taken = std::mem::replace(expr, Expr::Literal(axiomdb_types::Value::Null));
                *expr = resolve_expr(taken, &ctx)?;
            }
        }
        // Phase 11.25a: resolve JSONB SRF doc for UPDATE join sources.
        if let FromClause::JsonbSrf(srf) = &mut join.table {
            let taken = std::mem::replace(
                &mut srf.doc,
                Expr::Literal(axiomdb_types::Value::Null),
            );
            srf.doc = resolve_expr(taken, &ctx)?;
        }
        // Phase 21.22: resolve VALUES row exprs (no correlation) for UPDATE
        // join sources.
        if let FromClause::Values(vc) = &mut join.table {
            let empty_ctx = BindContext::empty();
            for row in &mut vc.rows {
                for e in row {
                    let taken =
                        std::mem::replace(e, Expr::Literal(axiomdb_types::Value::Null));
                    *e = resolve_expr(taken, &empty_ctx)?;
                }
            }
        }
        join.condition = match join.condition {
            JoinCondition::On(expr) => JoinCondition::On(resolve_expr(expr, &ctx)?),
            JoinCondition::Using(cols) => JoinCondition::Using(cols),
        };
        resolved_joins.push(join);
    }
    s.joins = resolved_joins;

    // Validate and resolve SET assignments.
    let mut resolved = Vec::with_capacity(s.assignments.len());
    for Assignment { column, value } in s.assignments {
        if !columns.iter().any(|c| c.name == column) {
            let available = columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(DbError::ColumnNotFound {
                name: column.clone(),
                table: format!("\"{}\" (available: {})", s.table.name, available),
            });
        }
        let value = resolve_expr(value, &ctx)?;
        resolved.push(Assignment { column, value });
    }
    s.assignments = resolved;

    s.where_clause = resolve_opt_expr(s.where_clause, &ctx)?;

    // Resolve ORDER BY expressions so column references have correct col_idx.
    let mut resolved_order = Vec::with_capacity(s.order_by.len());
    for mut item in s.order_by {
        item.expr = resolve_expr(item.expr, &ctx)?;
        resolved_order.push(item);
    }
    s.order_by = resolved_order;

    s.limit = resolve_opt_expr(s.limit, &ctx)?;

    // Phase 21.4 — resolve RETURNING against the target scope.
    let mut resolved_returning = Vec::with_capacity(s.returning.len());
    for item in std::mem::take(&mut s.returning) {
        resolved_returning.push(match item {
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => item,
            SelectItem::Expr { expr, alias } => SelectItem::Expr {
                expr: resolve_expr(expr, &ctx)?,
                alias,
            },
        });
    }
    s.returning = resolved_returning;

    Ok(s)
}

// ── DELETE ────────────────────────────────────────────────────────────────────

fn analyze_delete(
    mut s: DeleteStmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
) -> Result<DeleteStmt, DbError> {
    let mut reader = CatalogReader::new(storage, snapshot.clone())?;
    let table_def = resolve_dml_table(&mut reader, &s.table, default_database, default_schema)?;
    let columns = reader.list_columns(table_def.id)?;
    let ctx = if s.joins.is_empty() {
        let bound = BoundTable {
            alias: s.table.alias.clone(),
            name: s.table.name.clone(),
            columns,
            col_offset: 0,
        };
        BindContext {
            tables: vec![bound],
        }
    } else {
        build_context(
            &Some(FromClause::Table(s.table.clone())),
            &s.joins,
            storage,
            snapshot.clone(),
            default_database,
            default_schema,
        )?
    };

    let mut resolved_joins = Vec::with_capacity(s.joins.len());
    for mut join in s.joins {
        // Phase 11.20d4: resolve JSON_TABLE doc + PASSING for DELETE joins.
        if let FromClause::JsonTable(jt) = &mut join.table {
            let taken_doc = std::mem::replace(
                &mut jt.doc,
                Expr::Literal(axiomdb_types::Value::Null),
            );
            jt.doc = resolve_expr(taken_doc, &ctx)?;
            for (expr, _name) in &mut jt.passing {
                let taken = std::mem::replace(expr, Expr::Literal(axiomdb_types::Value::Null));
                *expr = resolve_expr(taken, &ctx)?;
            }
        }
        if let FromClause::JsonbSrf(srf) = &mut join.table {
            let taken = std::mem::replace(
                &mut srf.doc,
                Expr::Literal(axiomdb_types::Value::Null),
            );
            srf.doc = resolve_expr(taken, &ctx)?;
        }
        // Phase 21.22: VALUES rows for DELETE joins.
        if let FromClause::Values(vc) = &mut join.table {
            let empty_ctx = BindContext::empty();
            for row in &mut vc.rows {
                for e in row {
                    let taken =
                        std::mem::replace(e, Expr::Literal(axiomdb_types::Value::Null));
                    *e = resolve_expr(taken, &empty_ctx)?;
                }
            }
        }
        join.condition = match join.condition {
            JoinCondition::On(expr) => JoinCondition::On(resolve_expr(expr, &ctx)?),
            JoinCondition::Using(cols) => JoinCondition::Using(cols),
        };
        resolved_joins.push(join);
    }
    s.joins = resolved_joins;

    s.where_clause = resolve_opt_expr(s.where_clause, &ctx)?;

    // Resolve ORDER BY expressions so column references have correct col_idx.
    let mut resolved_order = Vec::with_capacity(s.order_by.len());
    for mut item in s.order_by {
        item.expr = resolve_expr(item.expr, &ctx)?;
        resolved_order.push(item);
    }
    s.order_by = resolved_order;

    s.limit = resolve_opt_expr(s.limit, &ctx)?;

    // Phase 21.4 — resolve RETURNING projection items against the target.
    let mut resolved_returning = Vec::with_capacity(s.returning.len());
    for item in s.returning {
        resolved_returning.push(match item {
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => item,
            SelectItem::Expr { expr, alias } => SelectItem::Expr {
                expr: resolve_expr(expr, &ctx)?,
                alias,
            },
        });
    }
    s.returning = resolved_returning;

    Ok(s)
}

// ── CREATE TABLE ──────────────────────────────────────────────────────────────

fn analyze_create_table(
    s: CreateTableStmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
) -> Result<CreateTableStmt, DbError> {
    let mut reader = CatalogReader::new(storage, snapshot)?;

    // Validate FK REFERENCES targets.
    for col_def in &s.columns {
        for constraint in &col_def.constraints {
            if let ColumnConstraint::References {
                table: ref_table,
                column: ref_col,
                ..
            } = constraint
            {
                let schema = default_schema;
                let exists = reader
                    .get_table_in_database(default_database, schema, ref_table)?
                    .is_some()
                    || reader
                        .get_table_in_database(default_database, "public", ref_table)?
                        .is_some();
                if !exists {
                    return Err(DbError::TableNotFound {
                        name: ref_table.clone(),
                    });
                }
                // If a specific column is referenced, validate it exists.
                if let Some(col_name) = ref_col {
                    let ref_table_def = reader
                        .get_table_in_database(default_database, default_schema, ref_table)?
                        .or_else(|| {
                            reader
                                .get_table_in_database(default_database, "public", ref_table)
                                .ok()
                                .flatten()
                        });
                    if let Some(ref_def) = ref_table_def {
                        let ref_cols = reader.list_columns(ref_def.id)?;
                        if !ref_cols.iter().any(|c| &c.name == col_name) {
                            return Err(DbError::ColumnNotFound {
                                name: col_name.clone(),
                                table: ref_table.clone(),
                            });
                        }
                    }
                }
            }
        }
    }

    Ok(s)
}

// ── DROP TABLE ────────────────────────────────────────────────────────────────

fn analyze_drop_table(
    s: DropTableStmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
) -> Result<DropTableStmt, DbError> {
    if s.if_exists {
        return Ok(s); // IF EXISTS: no validation needed
    }

    let mut reader = CatalogReader::new(storage, snapshot)?;
    for table_ref in &s.tables {
        let database = table_ref.database.as_deref().unwrap_or(default_database);
        let schema = table_ref.schema.as_deref().unwrap_or(default_schema);
        if table_ref.database.is_some() && !reader.database_exists(database)? {
            return Err(DbError::DatabaseNotFound {
                name: database.to_string(),
            });
        }
        let exists = reader
            .get_table_in_database(database, schema, &table_ref.name)?
            .is_some();
        if !exists {
            return Err(DbError::TableNotFound {
                name: table_ref.name.clone(),
            });
        }
    }

    Ok(s)
}

// ── CREATE INDEX ──────────────────────────────────────────────────────────────

fn analyze_create_index(
    s: CreateIndexStmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
) -> Result<CreateIndexStmt, DbError> {
    let schema = s.table.schema.as_deref().unwrap_or(default_schema);
    let mut reader = CatalogReader::new(storage, snapshot)?;

    let table_def = reader
        .get_table_in_database(default_database, schema, &s.table.name)?
        .ok_or_else(|| DbError::TableNotFound {
            name: s.table.name.clone(),
        })?;

    let columns = reader.list_columns(table_def.id)?;

    for idx_col in &s.columns {
        if !columns.iter().any(|c| c.name == idx_col.name) {
            let available = columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(DbError::ColumnNotFound {
                name: idx_col.name.clone(),
                table: format!("\"{}\" (available: {})", s.table.name, available),
            });
        }
    }

    Ok(s)
}

// ── ALTER TABLE ───────────────────────────────────────────────────────────────

fn analyze_alter_table(
    s: AlterTableStmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
) -> Result<AlterTableStmt, DbError> {
    let schema = s.table.schema.as_deref().unwrap_or(default_schema);
    let mut reader = CatalogReader::new(storage, snapshot)?;

    // Validate the target table exists.
    reader
        .get_table_in_database(default_database, schema, &s.table.name)?
        .ok_or_else(|| DbError::TableNotFound {
            name: s.table.name.clone(),
        })?;

    // Individual operations validated at execution time (Phase 4.22).
    // For now just validate the table exists.
    let _ = s.operations; // suppress unused warning
    Ok(s)
}
