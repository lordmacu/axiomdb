fn generated_column_constraint(
    col_def: &crate::ast::ColumnDef,
) -> Result<Option<(&Expr, GeneratedColumnKind)>, DbError> {
    let mut generated = None;
    for constraint in &col_def.constraints {
        if let ColumnConstraint::Generated { expr, kind } = constraint {
            if generated.is_some() {
                return Err(DbError::InvalidValue {
                    reason: format!(
                        "column '{}' has more than one generated column clause",
                        col_def.name
                    ),
                });
            }
            generated = Some((expr, *kind));
        }
    }
    Ok(generated)
}

/// Extracts the leaf scalar data type from a potentially-array DataType.
///
/// For `DataType::Array(Box::new(DataType::Int))`, returns `DataType::Int`.
/// For `DataType::Array(Box::new(DataType::Array(Box::new(DataType::Text))))`, returns `DataType::Text`.
/// For non-array types, returns the type unchanged.
fn extract_leaf_data_type(dt: &axiomdb_types::DataType) -> axiomdb_types::DataType {
    match dt {
        axiomdb_types::DataType::Array(inner) => extract_leaf_data_type(inner),
        other => other.clone(),
    }
}

fn validate_create_table_generated_columns(stmt: &CreateTableStmt) -> Result<(), DbError> {
    let mut generated_cols = std::collections::HashSet::new();
    let mut base_cols = std::collections::HashSet::new();
    for col in &stmt.columns {
        if generated_column_constraint(col)?.is_some() {
            generated_cols.insert(col.name.to_ascii_lowercase());
        } else {
            base_cols.insert(col.name.to_ascii_lowercase());
        }
    }

    for col in &stmt.columns {
        let Some((expr, kind)) = generated_column_constraint(col)? else {
            continue;
        };
        if matches!(kind, GeneratedColumnKind::Virtual) {
            return Err(DbError::NotImplemented {
                feature: "virtual generated columns".into(),
            });
        }
        if col
            .constraints
            .iter()
            .any(|c| matches!(c, ColumnConstraint::Default(_)))
        {
            return Err(DbError::InvalidValue {
                reason: format!("generated column '{}' cannot declare DEFAULT", col.name),
            });
        }
        if col
            .constraints
            .iter()
            .any(|c| matches!(c, ColumnConstraint::OnUpdate(_)))
        {
            return Err(DbError::InvalidValue {
                reason: format!("generated column '{}' cannot declare ON UPDATE", col.name),
            });
        }
        if col
            .constraints
            .iter()
            .any(|c| matches!(c, ColumnConstraint::AutoIncrement))
        {
            return Err(DbError::InvalidValue {
                reason: format!(
                    "generated column '{}' cannot declare AUTO_INCREMENT",
                    col.name
                ),
            });
        }
        validate_generated_expr_refs(
            expr,
            &stmt.table.name,
            &col.name,
            &base_cols,
            &generated_cols,
        )?;
    }

    Ok(())
}

fn validate_generated_expr_refs(
    expr: &Expr,
    table_name: &str,
    generated_name: &str,
    base_cols: &std::collections::HashSet<String>,
    generated_cols: &std::collections::HashSet<String>,
) -> Result<(), DbError> {
    match expr {
        Expr::Literal(_) => Ok(()),
        Expr::Column { name, .. } => {
            let key = name.to_ascii_lowercase();
            if key == generated_name.to_ascii_lowercase() {
                return Err(DbError::InvalidValue {
                    reason: format!("generated column '{generated_name}' cannot reference itself"),
                });
            }
            if generated_cols.contains(&key) {
                return Err(DbError::InvalidValue {
                    reason: format!(
                        "generated column '{generated_name}' cannot reference another generated column '{name}'"
                    ),
                });
            }
            if !base_cols.contains(&key) {
                return Err(DbError::ColumnNotFound {
                    name: name.clone(),
                    table: table_name.to_string(),
                });
            }
            Ok(())
        }
        Expr::UnaryOp { operand, .. } => validate_generated_expr_refs(
            operand,
            table_name,
            generated_name,
            base_cols,
            generated_cols,
        ),
        Expr::Collate { expr, .. } => validate_generated_expr_refs(
            expr,
            table_name,
            generated_name,
            base_cols,
            generated_cols,
        ),
        Expr::BinaryOp { left, right, .. } => {
            validate_generated_expr_refs(
                left,
                table_name,
                generated_name,
                base_cols,
                generated_cols,
            )?;
            validate_generated_expr_refs(
                right,
                table_name,
                generated_name,
                base_cols,
                generated_cols,
            )
        }
        Expr::IsNull { expr, .. } | Expr::IsBoolean { expr, .. } | Expr::Cast { expr, .. } => {
            validate_generated_expr_refs(
                expr,
                table_name,
                generated_name,
                base_cols,
                generated_cols,
            )
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            validate_generated_expr_refs(
                expr,
                table_name,
                generated_name,
                base_cols,
                generated_cols,
            )?;
            validate_generated_expr_refs(
                low,
                table_name,
                generated_name,
                base_cols,
                generated_cols,
            )?;
            validate_generated_expr_refs(
                high,
                table_name,
                generated_name,
                base_cols,
                generated_cols,
            )
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            validate_generated_expr_refs(
                expr,
                table_name,
                generated_name,
                base_cols,
                generated_cols,
            )?;
            validate_generated_expr_refs(
                pattern,
                table_name,
                generated_name,
                base_cols,
                generated_cols,
            )?;
            if let Some(escape) = escape {
                validate_generated_expr_refs(
                    escape,
                    table_name,
                    generated_name,
                    base_cols,
                    generated_cols,
                )?;
            }
            Ok(())
        }
        Expr::In { expr, list, .. } => {
            validate_generated_expr_refs(
                expr,
                table_name,
                generated_name,
                base_cols,
                generated_cols,
            )?;
            for item in list {
                validate_generated_expr_refs(
                    item,
                    table_name,
                    generated_name,
                    base_cols,
                    generated_cols,
                )?;
            }
            Ok(())
        }
        Expr::Function { args, .. } => {
            for arg in args {
                validate_generated_expr_refs(
                    arg,
                    table_name,
                    generated_name,
                    base_cols,
                    generated_cols,
                )?;
            }
            Ok(())
        }
        Expr::Window { .. } => Err(DbError::NotImplemented {
            feature: "window functions in generated columns".into(),
        }),
        Expr::Case {
            operand,
            when_thens,
            else_result,
        } => {
            if let Some(operand) = operand {
                validate_generated_expr_refs(
                    operand,
                    table_name,
                    generated_name,
                    base_cols,
                    generated_cols,
                )?;
            }
            for (when, then) in when_thens {
                validate_generated_expr_refs(
                    when,
                    table_name,
                    generated_name,
                    base_cols,
                    generated_cols,
                )?;
                validate_generated_expr_refs(
                    then,
                    table_name,
                    generated_name,
                    base_cols,
                    generated_cols,
                )?;
            }
            if let Some(else_result) = else_result {
                validate_generated_expr_refs(
                    else_result,
                    table_name,
                    generated_name,
                    base_cols,
                    generated_cols,
                )?;
            }
            Ok(())
        }
        Expr::SqlJsonQuery { doc, passing, .. } => {
            validate_generated_expr_refs(
                doc,
                table_name,
                generated_name,
                base_cols,
                generated_cols,
            )?;
            for (arg, _) in passing {
                validate_generated_expr_refs(
                    arg,
                    table_name,
                    generated_name,
                    base_cols,
                    generated_cols,
                )?;
            }
            Ok(())
        }
        Expr::Subquery(_) | Expr::InSubquery { .. } | Expr::Exists { .. } => {
            Err(DbError::NotImplemented {
                feature: "subqueries in generated columns".into(),
            })
        }
        Expr::GroupConcat { .. } | Expr::Grouping { .. } | Expr::ArrayAgg { .. } => Err(DbError::NotImplemented {
            feature: "aggregate expressions in generated columns".into(),
        }),
        // Phase 20.4 — ARRAY[expr, ...]: recurse into elements.
        Expr::ArrayConstructor { elements } => {
            for e in elements {
                validate_generated_expr_refs(
                    e,
                    table_name,
                    generated_name,
                    base_cols,
                    generated_cols,
                )?;
            }
            Ok(())
        }
        // Phase 20.4, Step 5 — array subscript: recurse into array and index.
        Expr::Subscript { array, index, slice } => {
            validate_generated_expr_refs(
                array,
                table_name,
                generated_name,
                base_cols,
                generated_cols,
            )?;
            validate_generated_expr_refs(
                index,
                table_name,
                generated_name,
                base_cols,
                generated_cols,
            )?;
            if let Some(s) = slice {
                validate_generated_expr_refs(
                    s,
                    table_name,
                    generated_name,
                    base_cols,
                    generated_cols,
                )?;
            }
            Ok(())
        }
        // Phase 20.4 — ANY/ALL: recurse into expr (comparison target) and array.
        Expr::AnyOf { expr, array, .. } | Expr::AllOf { expr, array, .. } => {
            validate_generated_expr_refs(expr, table_name, generated_name, base_cols, generated_cols)?;
            validate_generated_expr_refs(array, table_name, generated_name, base_cols, generated_cols)
        }
        Expr::OuterColumn { .. }
        | Expr::InsertValue { .. }
        | Expr::ExcludedValue { .. }
        | Expr::Param { .. }
        | Expr::Default => Err(DbError::InvalidValue {
            reason: format!(
                "unsupported expression in generated column '{generated_name}'"
            ),
        }),
    }
}

fn ensure_schema_exists_for_create(
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    database: &str,
    schema: &str,
) -> Result<(), DbError> {
    let snap = txn.active_snapshot(conn_txn);
    let mut reader = CatalogReader::new(storage, snap)?;
    if !reader.schema_exists(database, schema)? {
        CatalogWriter::new(storage, txn, conn_txn)?.create_schema(database, schema)?;
    }
    Ok(())
}

fn execute_create_table(
    stmt: CreateTableStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    database: &str,
) -> Result<QueryResult, DbError> {
    let schema = stmt.table.schema.as_deref().unwrap_or("public");
    let primary_key = collect_create_table_primary_key(&stmt)?;
    let unique_indexes = collect_create_table_unique_indexes(&stmt)?;
    let non_unique_indexes = collect_create_table_non_unique_indexes(&stmt)?;
    let exclusion_constraints = collect_create_table_exclusion_constraints(&stmt)?;
    let primary_key_cols: std::collections::HashSet<u16> = primary_key
        .as_ref()
        .map(|pk| pk.columns.iter().map(|c| c.col_idx).collect())
        .unwrap_or_default();
    let storage_layout = if primary_key.is_some() {
        axiomdb_catalog::schema::TableStorageLayout::Clustered
    } else {
        axiomdb_catalog::schema::TableStorageLayout::Heap
    };
    validate_create_table_generated_columns(&stmt)?;

    if stmt.persistence == axiomdb_catalog::TablePersistence::Temporary {
        ensure_schema_exists_for_create(storage, txn, conn_txn, database, schema)?;
    } else {
        // Permanent tables: schema must exist explicitly; reject unknown schemas.
        let snap = txn.active_snapshot(conn_txn);
        let mut reader = CatalogReader::new(storage, snap)?;
        if !reader.schema_exists(database, schema)? {
            return Err(DbError::SchemaNotFound { name: schema.to_string() });
        }
    }

    let enum_type_names = resolve_create_table_enum_types(&stmt, storage, txn, conn_txn, schema)?;

    // Check existence before constructing CatalogWriter (avoids double mutable borrow).
    {
        let mut resolver = make_resolver_with_database(storage, txn, Some(conn_txn), database)?;
        if resolver.table_exists(Some(schema), &stmt.table.name)? {
            if stmt.if_not_exists {
                return Ok(QueryResult::Empty);
            }
            return Err(DbError::TableAlreadyExists {
                schema: schema.to_string(),
                name: stmt.table.name.clone(),
            });
        }
    } // resolver dropped here — releases immutable borrow on storage

    let mut writer = CatalogWriter::new(storage, txn, conn_txn)?;
    let table_def = writer.create_table_with_options(
        schema,
        &stmt.table.name,
        storage_layout,
        stmt.immutable,
        stmt.persistence,
        stmt.collation.clone(),
    )?;
    let table_id = table_def.id;
    if database != DEFAULT_DATABASE_NAME {
        writer.bind_table_to_database(table_id, database)?;
    }

    // Collect inline REFERENCES constraints for processing after all columns are created.
    // We must create all columns first so col_idx values are stable.
    let mut inline_fk_specs: Vec<InlineFkSpec> = Vec::new();

    for (i, col_def) in stmt.columns.iter().enumerate() {
        let type_len = col_def.type_len;
        let nullable = !col_def
            .constraints
            .iter()
            .any(|c| matches!(c, ColumnConstraint::NotNull))
            && !primary_key_cols.contains(&(i as u16));
        let auto_increment = col_def
            .constraints
            .iter()
            .any(|c| matches!(c, ColumnConstraint::AutoIncrement));
        if let Some(refs) = col_def.constraints.iter().find_map(|c| {
            if let ColumnConstraint::References {
                table,
                column,
                on_delete,
                on_update,
                deferrability,
            } = c
            {
                Some((
                    table.clone(),
                    column.clone(),
                    *on_delete,
                    *on_update,
                    *deferrability,
                ))
            } else {
                None
            }
        }) {
            inline_fk_specs.push((i as u16, col_def.name.clone(), refs));
        }

        let default_expr = col_def.constraints.iter().find_map(|c| match c {
            ColumnConstraint::Default(expr) => Some(crate::expr_to_sql::expr_to_sql_string(expr)),
            _ => None,
        });
        let on_update_expr = col_def.constraints.iter().find_map(|c| match c {
            ColumnConstraint::OnUpdate(expr) => Some(crate::expr_to_sql::expr_to_sql_string(expr)),
            _ => None,
        });
        let generated = generated_column_constraint(col_def)?;
        let generated_expr = generated.map(|(expr, _)| crate::expr_to_sql::expr_to_sql_string(expr));
        let generated_stored = generated
            .map(|(_, kind)| matches!(kind, GeneratedColumnKind::Stored))
            .unwrap_or(false);

        // Handle array columns: extract leaf element type and ndims
        let (col_type, array_element_type, array_ndims) = if let Some(ndims) = col_def.array_ndims {
            // This is an array column
            let leaf_type = extract_leaf_data_type(&col_def.data_type);
            let element_ct = datatype_to_column_type(&leaf_type)?;
            (ColumnType::Array, Some(element_ct), Some(ndims))
        } else {
            let ct = datatype_to_column_type(&col_def.data_type)?;
            (ct, None, None)
        };

        writer.create_column(CatalogColumnDef {
            table_id,
            col_idx: i as u16,
            name: col_def.name.clone(),
            col_type,
            nullable,
            auto_increment,
            type_len,
            is_fixed_len: col_def.is_char,
            default_expr,
            on_update_expr,
            generated_expr,
            collation: col_def.collation.clone(),
            generated_stored,
            enum_type_name: enum_type_names[i].clone(),
            array_element_type,
            array_ndims,
        })?;
    }

    {
        use axiomdb_index::page_layout::{cast_leaf_mut, NULL_PAGE};

        let create_empty_index = |index_name: String,
                                  columns: Vec<IndexColumnDef>,
                                  is_unique: bool,
                                  is_primary: bool,
                                  root_override: Option<u64>,
                                  storage: &dyn StorageEngine,
                                  txn: &TxnManager,
                                  conn_txn: &mut axiomdb_wal::ConnectionTxn|
         -> Result<u32, DbError> {
            let root_page_id = match root_override {
                Some(root_page_id) => root_page_id,
                None => {
                    let root_page_id = storage.alloc_page(PageType::Index)?;
                    let mut page = Page::new(PageType::Index, root_page_id);
                    let leaf = cast_leaf_mut(&mut page);
                    leaf.is_leaf = 1;
                    leaf.set_num_keys(0);
                    leaf.set_next_leaf(NULL_PAGE);
                    page.update_checksum();
                    storage.write_page(root_page_id, &page)?;
                    root_page_id
                }
            };

            CatalogWriter::new(storage, txn, conn_txn)?.create_index(IndexDef {
                index_id: 0,
                table_id,
                name: index_name,
                root_page_id,
                is_unique,
                fillfactor: 90,
                is_primary,
                columns,
                predicate: None,
                is_fk_index: false,
                include_columns: vec![],
                index_type: 0,
                pages_per_range: 128,
            })
        };

        if let Some(pk_spec) = primary_key {
            let idx_id = create_empty_index(
                pk_spec.name,
                pk_spec.columns,
                true,
                true,
                Some(table_def.root_page_id),
                storage,
                txn,
                conn_txn,
            )?;
            let _ = idx_id;
        }

        for unique_spec in unique_indexes {
            let idx_id = create_empty_index(
                unique_spec.name,
                unique_spec.columns,
                true,
                false,
                None,
                storage,
                txn,
                conn_txn,
            )?;
            let _ = idx_id;
        }

        // Non-unique inline INDEX/KEY constraints (MySQL extension).
        for idx_spec in non_unique_indexes {
            let idx_id = create_empty_index(
                idx_spec.name,
                idx_spec.columns,
                false,
                false,
                None,
                storage,
                txn,
                conn_txn,
            )?;
            let _ = idx_id;
        }

        for exclusion_spec in exclusion_constraints {
            let idx_id = create_empty_index(
                exclusion_spec.helper_index_name,
                exclusion_spec.index_columns,
                true,
                false,
                None,
                storage,
                txn,
                conn_txn,
            )?;
            CatalogWriter::new(storage, txn, conn_txn)?.create_constraint(
                axiomdb_catalog::schema::ConstraintDef {
                    constraint_id: 0,
                    table_id,
                    name: exclusion_spec.constraint_name,
                    check_expr: String::new(),
                    kind: axiomdb_catalog::schema::ConstraintKind::Exclusion,
                    owned_index_id: idx_id,
                    exclude_elements: exclusion_spec.exclude_elements,
                },
            )?;
        }
    }

    for (
        child_col_idx,
        child_col_name,
        (ref_table, ref_col, on_delete, on_update, deferrability),
    ) in
        inline_fk_specs
    {
        persist_fk_constraint(
            table_id,
            &stmt.table.name,
            database,
            child_col_idx,
            &child_col_name,
            &ref_table,
            ref_col.as_deref(),
            ast_fk_action_to_catalog(on_delete),
            ast_fk_action_to_catalog(on_update),
            deferrability,
            None,
            storage,
            txn,
            conn_txn,
        )?;
    }

    // Phase 21.6 — persist CHECK constraints declared at column level or
    // table level. Enforcement is driven by insert_helpers::check_row_constraints
    // reading the persisted check_expr from axiom_constraints.
    let mut check_ordinal = 1usize;
    for col_def in &stmt.columns {
        for cc in &col_def.constraints {
            if let crate::ast::ColumnConstraint::Check(expr) = cc {
                let check_name = format!("axiom_check_{}_{}", col_def.name, check_ordinal);
                check_ordinal += 1;
                let check_expr_str = expr_to_sql_string(expr);
                CatalogWriter::new(storage, txn, conn_txn)?.create_constraint(
                    axiomdb_catalog::schema::ConstraintDef {
                        constraint_id: 0,
                        table_id,
                        name: check_name,
                        check_expr: check_expr_str,
                        kind: axiomdb_catalog::schema::ConstraintKind::Check,
                        owned_index_id: 0,
                        exclude_elements: vec![],
                    },
                )?;
            }
        }
    }
    for tc in &stmt.table_constraints {
        if let crate::ast::TableConstraint::Check { name, expr } = tc {
            let check_name = name.clone().unwrap_or_else(|| {
                let n = format!("axiom_check_tbl_{}", check_ordinal);
                check_ordinal += 1;
                n
            });
            let check_expr_str = expr_to_sql_string(expr);
            CatalogWriter::new(storage, txn, conn_txn)?.create_constraint(
                axiomdb_catalog::schema::ConstraintDef {
                    constraint_id: 0,
                    table_id,
                    name: check_name,
                    check_expr: check_expr_str,
                    kind: axiomdb_catalog::schema::ConstraintKind::Check,
                    owned_index_id: 0,
                    exclude_elements: vec![],
                },
            )?;
        }
    }

    for tc in &stmt.table_constraints {
        if let crate::ast::TableConstraint::ForeignKey {
            name,
            columns,
            ref_table,
            ref_columns,
            on_delete,
            on_update,
            deferrability,
        } = tc
        {
            let snap = txn.active_snapshot(conn_txn);
            let child_col_idxs: Vec<u16> = {
                let mut reader = CatalogReader::new(storage, snap.clone())?;
                let cols = reader.list_columns(table_id)?;
                columns
                    .iter()
                    .map(|name| {
                        cols.iter()
                            .find(|c| &c.name == name)
                            .map(|c| c.col_idx)
                            .ok_or_else(|| DbError::ColumnNotFound {
                                name: name.clone(),
                                table: stmt.table.name.clone(),
                            })
                    })
                    .collect::<Result<_, _>>()?
            };
            if columns.len() == 1 {
                let ref_col = ref_columns.first().map(|s| s.as_str());
                persist_fk_constraint(
                    table_id,
                    &stmt.table.name,
                    database,
                    child_col_idxs[0],
                    &columns[0],
                    ref_table,
                    ref_col,
                    ast_fk_action_to_catalog(*on_delete),
                    ast_fk_action_to_catalog(*on_update),
                    *deferrability,
                    name.as_deref(),
                    storage,
                    txn,
                    conn_txn,
                )?;
            } else {
                persist_composite_fk_constraint(
                    table_id,
                    &stmt.table.name,
                    database,
                    &child_col_idxs,
                    columns,
                    ref_table,
                    ref_columns,
                    ast_fk_action_to_catalog(*on_delete),
                    ast_fk_action_to_catalog(*on_update),
                    *deferrability,
                    name.as_deref(),
                    storage,
                    txn,
                    conn_txn,
                )?;
            }
        }
    }

    Ok(QueryResult::Empty)
}

fn resolve_create_table_enum_types(
    stmt: &CreateTableStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &axiomdb_wal::ConnectionTxn,
    default_schema: &str,
) -> Result<Vec<Option<String>>, DbError> {
    let snap = txn.active_snapshot(conn_txn);
    let mut reader = CatalogReader::new(storage, snap)?;
    stmt.columns
        .iter()
        .map(|col| {
            let Some(type_ref) = &col.declared_type_name else {
                return Ok(None);
            };
            if type_ref.database.is_some() {
                return Err(DbError::InvalidValue {
                    reason: format!(
                        "enum type '{}' cannot be qualified with a database",
                        type_ref.name
                    ),
                });
            }
            let type_schema = type_ref.schema.as_deref().unwrap_or(default_schema);
            if reader.get_enum_type(type_schema, &type_ref.name)?.is_none() {
                return Err(DbError::InvalidValue {
                    reason: format!("enum type '{}.{}' does not exist", type_schema, type_ref.name),
                });
            }
            Ok(Some(format!("{}.{}", type_schema, type_ref.name)))
        })
        .collect()
}

#[derive(Debug, Clone)]
struct CreateTableIndexSpec {
    name: String,
    columns: Vec<IndexColumnDef>,
}

#[derive(Debug, Clone)]
struct CreateTableExclusionSpec {
    constraint_name: String,
    helper_index_name: String,
    index_columns: Vec<IndexColumnDef>,
    exclude_elements: Vec<axiomdb_catalog::ExclusionElementDef>,
}

type CreateTableExclusionResolution = (
    Vec<String>,
    Vec<IndexColumnDef>,
    Vec<axiomdb_catalog::ExclusionElementDef>,
);

type ExistingTableExclusionResolution = (
    Vec<String>,
    Vec<crate::ast::IndexColumn>,
    Vec<axiomdb_catalog::ExclusionElementDef>,
);

fn exclusion_operator_to_feature(operator: crate::ast::ExclusionOperator) -> &'static str {
    match operator {
        crate::ast::ExclusionOperator::Eq => "=",
        crate::ast::ExclusionOperator::NotEq => "<>",
        crate::ast::ExclusionOperator::Lt => "<",
        crate::ast::ExclusionOperator::LtEq => "<=",
        crate::ast::ExclusionOperator::Gt => ">",
        crate::ast::ExclusionOperator::GtEq => ">=",
        crate::ast::ExclusionOperator::Overlaps => "&&",
    }
}

fn ast_exclusion_operator_to_catalog(
    operator: crate::ast::ExclusionOperator,
) -> axiomdb_catalog::ConstraintOperator {
    match operator {
        crate::ast::ExclusionOperator::Eq => axiomdb_catalog::ConstraintOperator::Eq,
        crate::ast::ExclusionOperator::NotEq => axiomdb_catalog::ConstraintOperator::NotEq,
        crate::ast::ExclusionOperator::Lt => axiomdb_catalog::ConstraintOperator::Lt,
        crate::ast::ExclusionOperator::LtEq => axiomdb_catalog::ConstraintOperator::LtEq,
        crate::ast::ExclusionOperator::Gt => axiomdb_catalog::ConstraintOperator::Gt,
        crate::ast::ExclusionOperator::GtEq => axiomdb_catalog::ConstraintOperator::GtEq,
        crate::ast::ExclusionOperator::Overlaps => axiomdb_catalog::ConstraintOperator::Overlaps,
    }
}

fn default_exclusion_constraint_name(table_name: &str, columns: &[String]) -> String {
    format!("{}_{}_excl", table_name, columns.join("_"))
}

fn exclusion_helper_index_name(constraint_name: &str) -> String {
    format!("__axiom_excl_idx_{constraint_name}")
}

fn validate_exclusion_access_method(using: &str) -> Result<(), DbError> {
    if using.eq_ignore_ascii_case("btree") {
        Ok(())
    } else {
        Err(DbError::NotImplemented {
            feature: format!("EXCLUDE USING {using} exclusion constraints"),
        })
    }
}

fn validate_exclusion_predicate(predicate: &Option<Expr>) -> Result<(), DbError> {
    if predicate.is_some() {
        return Err(DbError::NotImplemented {
            feature: "exclusion constraint predicates".into(),
        });
    }
    Ok(())
}

fn resolve_exclusion_elements_create_table(
    stmt: &CreateTableStmt,
    elements: &[crate::ast::ExclusionElement],
) -> Result<CreateTableExclusionResolution, DbError> {
    if elements.is_empty() {
        return Err(DbError::InvalidValue {
            reason: "EXCLUDE constraint requires at least one element".into(),
        });
    }

    let mut column_names = Vec::with_capacity(elements.len());
    let mut index_columns = Vec::with_capacity(elements.len());
    let mut exclude_elements = Vec::with_capacity(elements.len());

    for element in elements {
        let col_name = match &element.target {
            crate::ast::ExclusionElementTarget::Column(name) => name.clone(),
            crate::ast::ExclusionElementTarget::Expr(_) => {
                return Err(DbError::NotImplemented {
                    feature: "expression exclusion elements".into(),
                })
            }
        };
        if !matches!(element.operator, crate::ast::ExclusionOperator::Eq) {
            return Err(DbError::NotImplemented {
                feature: format!(
                    "EXCLUDE ... WITH {} operators",
                    exclusion_operator_to_feature(element.operator)
                ),
            });
        }
        let (col_idx, _) = stmt
            .columns
            .iter()
            .enumerate()
            .find(|(_, c)| c.name == col_name)
            .ok_or_else(|| DbError::ColumnNotFound {
                name: col_name.clone(),
                table: stmt.table.name.clone(),
            })?;
        column_names.push(col_name);
        index_columns.push(IndexColumnDef {
            col_idx: col_idx as u16,
            order: CatalogSortOrder::Asc,
            expr: None,
        });
        exclude_elements.push(axiomdb_catalog::ExclusionElementDef {
            col_idx: col_idx as u16,
            operator: ast_exclusion_operator_to_catalog(element.operator),
        });
    }

    Ok((column_names, index_columns, exclude_elements))
}

fn resolve_exclusion_elements_existing(
    table_name: &str,
    columns_arg: &[axiomdb_catalog::schema::ColumnDef],
    elements: &[crate::ast::ExclusionElement],
) -> Result<ExistingTableExclusionResolution, DbError> {
    if elements.is_empty() {
        return Err(DbError::InvalidValue {
            reason: "EXCLUDE constraint requires at least one element".into(),
        });
    }

    let mut column_names = Vec::with_capacity(elements.len());
    let mut ast_columns = Vec::with_capacity(elements.len());
    let mut exclude_elements = Vec::with_capacity(elements.len());

    for element in elements {
        let col_name = match &element.target {
            crate::ast::ExclusionElementTarget::Column(name) => name.clone(),
            crate::ast::ExclusionElementTarget::Expr(_) => {
                return Err(DbError::NotImplemented {
                    feature: "expression exclusion elements".into(),
                })
            }
        };
        if !matches!(element.operator, crate::ast::ExclusionOperator::Eq) {
            return Err(DbError::NotImplemented {
                feature: format!(
                    "EXCLUDE ... WITH {} operators",
                    exclusion_operator_to_feature(element.operator)
                ),
            });
        }
        let col = columns_arg
            .iter()
            .find(|c| c.name == col_name)
            .ok_or_else(|| DbError::ColumnNotFound {
                name: col_name.clone(),
                table: table_name.to_string(),
            })?;
        column_names.push(col_name.clone());
        ast_columns.push(crate::ast::IndexColumn {
            name: col_name,
            order: crate::ast::SortOrder::Asc,
            expr: None,
        });
        exclude_elements.push(axiomdb_catalog::ExclusionElementDef {
            col_idx: col.col_idx,
            operator: ast_exclusion_operator_to_catalog(element.operator),
        });
    }

    Ok((column_names, ast_columns, exclude_elements))
}

fn collect_create_table_exclusion_constraints(
    stmt: &CreateTableStmt,
) -> Result<Vec<CreateTableExclusionSpec>, DbError> {
    let mut exclusions = Vec::new();

    for tc in &stmt.table_constraints {
        if let crate::ast::TableConstraint::Exclude {
            name,
            using,
            elements,
            predicate,
        } = tc
        {
            validate_exclusion_access_method(using)?;
            validate_exclusion_predicate(predicate)?;
            let (column_names, index_columns, exclude_elements) =
                resolve_exclusion_elements_create_table(stmt, elements)?;
            let constraint_name = name.clone().unwrap_or_else(|| {
                default_exclusion_constraint_name(&stmt.table.name, &column_names)
            });
            exclusions.push(CreateTableExclusionSpec {
                helper_index_name: exclusion_helper_index_name(&constraint_name),
                constraint_name,
                index_columns,
                exclude_elements,
            });
        }
    }

    Ok(exclusions)
}

fn resolve_create_table_index_columns(
    stmt: &CreateTableStmt,
    columns: &[String],
) -> Result<Vec<IndexColumnDef>, DbError> {
    if columns.is_empty() {
        return Err(DbError::InvalidValue {
            reason: "PRIMARY KEY / UNIQUE requires at least one column".into(),
        });
    }

    columns
        .iter()
        .map(|col_name| {
            let (col_idx, _) = stmt
                .columns
                .iter()
                .enumerate()
                .find(|(_, c)| c.name == *col_name)
                .ok_or_else(|| DbError::ColumnNotFound {
                    name: col_name.clone(),
                    table: stmt.table.name.clone(),
                })?;
            Ok(IndexColumnDef {
                col_idx: col_idx as u16,
                order: CatalogSortOrder::Asc,
                expr: None,
            })
        })
        .collect()
}

fn collect_create_table_primary_key(
    stmt: &CreateTableStmt,
) -> Result<Option<CreateTableIndexSpec>, DbError> {
    let inline_pk_cols: Vec<(u16, String)> = stmt
        .columns
        .iter()
        .enumerate()
        .filter(|(_, col_def)| {
            col_def
                .constraints
                .iter()
                .any(|c| matches!(c, ColumnConstraint::PrimaryKey))
        })
        .map(|(idx, col_def)| (idx as u16, col_def.name.clone()))
        .collect();

    let mut table_pk = None;
    for tc in &stmt.table_constraints {
        if let crate::ast::TableConstraint::PrimaryKey { name, columns } = tc {
            if table_pk.is_some() || !inline_pk_cols.is_empty() {
                return Err(DbError::InvalidValue {
                    reason: "multiple PRIMARY KEY constraints are not allowed".into(),
                });
            }
            table_pk = Some(CreateTableIndexSpec {
                name: name
                    .clone()
                    .unwrap_or_else(|| format!("{}_pkey", stmt.table.name)),
                columns: resolve_create_table_index_columns(stmt, columns)?,
            });
        }
    }

    if !inline_pk_cols.is_empty() {
        if inline_pk_cols.len() > 1 {
            return Err(DbError::InvalidValue {
                reason:
                    "multiple inline PRIMARY KEY columns are not allowed; use PRIMARY KEY (...)"
                        .into(),
            });
        }
        return Ok(Some(CreateTableIndexSpec {
            name: format!("{}_pkey", stmt.table.name),
            columns: vec![IndexColumnDef {
                col_idx: inline_pk_cols[0].0,
                order: CatalogSortOrder::Asc,
                expr: None,
            }],
        }));
    }

    Ok(table_pk)
}

fn collect_create_table_unique_indexes(
    stmt: &CreateTableStmt,
) -> Result<Vec<CreateTableIndexSpec>, DbError> {
    let mut unique_indexes = Vec::new();

    for (idx, col_def) in stmt.columns.iter().enumerate() {
        if col_def
            .constraints
            .iter()
            .any(|c| matches!(c, crate::ast::ColumnConstraint::Unique))
        {
            unique_indexes.push(CreateTableIndexSpec {
                name: format!("{}_{}_unique", stmt.table.name, col_def.name),
                columns: vec![IndexColumnDef {
                    col_idx: idx as u16,
                    order: CatalogSortOrder::Asc,
                    expr: None,
                }],
            });
        }
    }

    for tc in &stmt.table_constraints {
        if let crate::ast::TableConstraint::Unique { name, columns } = tc {
            let generated_name = if columns.len() == 1 {
                format!("{}_{}_unique", stmt.table.name, columns[0])
            } else {
                format!("{}_{}_unique", stmt.table.name, columns.join("_"))
            };
            unique_indexes.push(CreateTableIndexSpec {
                name: name.clone().unwrap_or(generated_name),
                columns: resolve_create_table_index_columns(stmt, columns)?,
            });
        }
    }

    Ok(unique_indexes)
}

fn collect_create_table_non_unique_indexes(
    stmt: &CreateTableStmt,
) -> Result<Vec<CreateTableIndexSpec>, DbError> {
    let mut indexes = Vec::new();
    for tc in &stmt.table_constraints {
        if let crate::ast::TableConstraint::Index { name, columns } = tc {
            let generated_name = if columns.len() == 1 {
                format!("{}_{}_idx", stmt.table.name, columns[0])
            } else {
                format!("{}_{}_idx", stmt.table.name, columns.join("_"))
            };
            indexes.push(CreateTableIndexSpec {
                name: name.clone().unwrap_or(generated_name),
                columns: resolve_create_table_index_columns(stmt, columns)?,
            });
        }
    }
    Ok(indexes)
}

// ── FK helpers ────────────────────────────────────────────────────────────────

/// Converts an AST [`ForeignKeyAction`] to the catalog [`FkAction`] used in `FkDef`.
fn ast_fk_action_to_catalog(action: crate::ast::ForeignKeyAction) -> axiomdb_catalog::FkAction {
    use crate::ast::ForeignKeyAction;
    use axiomdb_catalog::FkAction;
    match action {
        ForeignKeyAction::NoAction => FkAction::NoAction,
        ForeignKeyAction::Restrict => FkAction::Restrict,
        ForeignKeyAction::Cascade => FkAction::Cascade,
        ForeignKeyAction::SetNull => FkAction::SetNull,
        ForeignKeyAction::SetDefault => FkAction::SetDefault,
    }
}

fn reject_fk_persistence(
    persistence: axiomdb_catalog::TablePersistence,
    parent_side: bool,
) -> Result<(), DbError> {
    match (persistence, parent_side) {
        (axiomdb_catalog::TablePersistence::Permanent, _) => Ok(()),
        (axiomdb_catalog::TablePersistence::Temporary, false) => Err(DbError::NotImplemented {
            feature: "foreign keys on temporary tables".into(),
        }),
        (axiomdb_catalog::TablePersistence::Unlogged, false) => Err(DbError::NotImplemented {
            feature: "foreign keys on unlogged tables".into(),
        }),
        (axiomdb_catalog::TablePersistence::Temporary, true) => Err(DbError::NotImplemented {
            feature: "foreign keys referencing temporary tables".into(),
        }),
        (axiomdb_catalog::TablePersistence::Unlogged, true) => Err(DbError::NotImplemented {
            feature: "foreign keys referencing unlogged tables".into(),
        }),
    }
}

fn resolve_fk_parent_table(
    database: &str,
    ref_table: &str,
    child_schema: &str,
    storage: &dyn StorageEngine,
    snap: axiomdb_core::TransactionSnapshot,
) -> Result<axiomdb_catalog::TableDef, DbError> {
    let mut reader = CatalogReader::new(storage, snap)?;
    let matches: Vec<_> = reader
        .list_tables_owned_by_database(database)?
        .into_iter()
        .filter(|table| table.table_name == ref_table)
        .collect();

    if let Some(table) = matches
        .iter()
        .find(|table| table.schema_name == child_schema && table.persistence == axiomdb_catalog::TablePersistence::Temporary)
    {
        reject_fk_persistence(table.persistence, true)?;
    }

    if let Some(table) = matches.iter().find(|table| {
        table.schema_name == "public"
            && table.persistence == axiomdb_catalog::TablePersistence::Unlogged
    }) {
        reject_fk_persistence(table.persistence, true)?;
    }

    if let Some(table) = matches.iter().find(|table| table.schema_name == "public") {
        return Ok(table.clone());
    }

    if let Some(table) = matches
        .iter()
        .find(|table| table.persistence == axiomdb_catalog::TablePersistence::Temporary)
    {
        reject_fk_persistence(table.persistence, true)?;
    }

    Err(DbError::TableNotFound {
        name: ref_table.to_string(),
    })
}

/// Validates and persists a single FK constraint definition.
///
/// Called from `execute_create_table` (inline `REFERENCES` and table-level
/// `FOREIGN KEY`) and from `alter_add_constraint`.
///
/// # Steps
/// 1. Resolve parent table and referenced column (defaults to PK if unspecified).
/// 2. Verify parent column has a PRIMARY KEY or UNIQUE index.
/// 3. Auto-generate constraint name if not provided.
/// 4. Check uniqueness of constraint name on this child table.
/// 5. Create an index on the FK column in the child table if none exists.
/// 6. Persist `FkDef` in `axiom_foreign_keys`.
#[allow(clippy::too_many_arguments)]
fn persist_fk_constraint(
    child_table_id: u32,
    child_table_name: &str,
    database: &str,
    child_col_idx: u16,
    child_col_name: &str,
    ref_table: &str,
    ref_col: Option<&str>,
    on_delete: axiomdb_catalog::FkAction,
    on_update: axiomdb_catalog::FkAction,
    deferrability: crate::ast::ConstraintDeferrability,
    fk_name: Option<&str>,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
) -> Result<(), DbError> {
    use axiomdb_catalog::FkDef;

    let snap = txn.active_snapshot(conn_txn);

    let child_table_def = {
        let mut reader = CatalogReader::new(storage, snap.clone())?;
        reader
            .get_table_by_id(child_table_id)?
            .ok_or(DbError::CatalogTableNotFound {
                table_id: child_table_id,
            })?
    };
    reject_fk_persistence(child_table_def.persistence, false)?;

    // 1. Resolve parent table.
    let parent_def =
        resolve_fk_parent_table(database, ref_table, &child_table_def.schema_name, storage, snap.clone())?;
    reject_fk_persistence(parent_def.persistence, true)?;

    // 2. Find the referenced column in the parent table.
    let parent_cols = {
        let mut reader = CatalogReader::new(storage, snap.clone())?;
        reader.list_columns(parent_def.id)?
    };
    let parent_col_idx: u16 = if let Some(col_name) = ref_col {
        parent_cols
            .iter()
            .find(|c| c.name == col_name)
            .map(|c| c.col_idx)
            .ok_or_else(|| DbError::ColumnNotFound {
                name: col_name.to_string(),
                table: ref_table.to_string(),
            })?
    } else {
        // Default: use the leading column of the primary key index.
        let parent_indexes = {
            let mut reader = CatalogReader::new(storage, snap.clone())?;
            reader.list_indexes(parent_def.id)?
        };
        let pk_idx = parent_indexes
            .iter()
            .find(|i| i.is_primary && !i.columns.is_empty())
            .ok_or_else(|| DbError::ForeignKeyNoParentIndex {
                table: ref_table.to_string(),
                column: "<primary key>".to_string(),
            })?;
        pk_idx.columns[0].col_idx
    };

    // 3. Verify the parent column has a PRIMARY KEY or UNIQUE index covering it.
    {
        let mut reader = CatalogReader::new(storage, snap.clone())?;
        let parent_indexes = reader.list_indexes(parent_def.id)?;
        let has_unique = parent_indexes.iter().any(|i| {
            (i.is_primary || i.is_unique)
                && i.columns.len() == 1
                && i.columns[0].col_idx == parent_col_idx
        });
        if !has_unique {
            let col_name = parent_cols
                .iter()
                .find(|c| c.col_idx == parent_col_idx)
                .map(|c| c.name.clone())
                .unwrap_or_else(|| format!("col_{parent_col_idx}"));
            return Err(DbError::ForeignKeyNoParentIndex {
                table: ref_table.to_string(),
                column: col_name,
            });
        }
    }

    // 4. Auto-generate FK name if not provided.
    let constraint_name: String = fk_name
        .map(|n| n.to_string())
        .unwrap_or_else(|| format!("fk_{child_table_name}_{child_col_name}_{ref_table}"));

    // 5. Check FK name uniqueness on this child table.
    {
        let mut reader = CatalogReader::new(storage, snap.clone())?;
        if reader
            .get_fk_by_name(child_table_id, &constraint_name)?
            .is_some()
        {
            return Err(DbError::Other(format!(
                "foreign key constraint '{constraint_name}' already exists on table \
                 '{child_table_name}'"
            )));
        }
    }

    // 6. FK auto-index on child table (Phase 6.9).
    use axiomdb_catalog::{IndexColumnDef as CatIndexColumnDef, SortOrder as CatSortOrder};
    //
    // Uses composite keys: encode_index_key(&[fk_val]) ++ encode_rid(rid) (10 bytes).
    // Every entry is globally unique even when multiple rows share the same FK value —
    // the InnoDB approach (appending PK as tiebreaker). This enables O(log n)
    // range scans for RESTRICT/CASCADE/SET NULL enforcement.
    let fk_index_id: u32 = {
        use axiomdb_index::page_layout::{cast_leaf_mut, NULL_PAGE};
        use std::sync::atomic::{AtomicU64, Ordering};

        if child_table_def.is_clustered() {
            // Clustered child table: FK auto-index (composite heap RID key) is
            // incompatible with the clustered layout. Enforcement always falls back
            // to a full scan via scan_clustered_table (fk_index_id = 0 path).
            0
        } else {
            // Check if child already has a suitable covering index on child_col_idx
            // (user-provided, not an FK auto-index).
            let existing_covers = {
                let mut reader = CatalogReader::new(storage, snap.clone())?;
                reader.list_indexes(child_table_id)?.into_iter().any(|i| {
                    !i.is_fk_index && !i.columns.is_empty() && i.columns[0].col_idx == child_col_idx
                })
            };

            if existing_covers {
                0 // reuse existing user-provided index; will not be dropped with FK
            } else {
                // Build FK auto-index with composite keys from existing child rows.
                let root_page_id = storage.alloc_page(PageType::Index)?;
                {
                    let mut page = Page::new(PageType::Index, root_page_id);
                    let leaf = cast_leaf_mut(&mut page);
                    leaf.is_leaf = 1;
                    leaf.set_num_keys(0);
                    leaf.set_next_leaf(NULL_PAGE);
                    page.update_checksum();
                    storage.write_page(root_page_id, &page)?;
                }
                let root_pid = AtomicU64::new(root_page_id);

                let child_cols = {
                    let mut reader = CatalogReader::new(storage, snap.clone())?;
                    reader.list_columns(child_table_id)?
                };

                // Insert composite key entry for every existing child row.
                let rows = TableEngine::scan_table(
                    storage,
                    &child_table_def,
                    &child_cols,
                    snap,
                    None,
                )?;
                for (rid, row_vals) in rows {
                    let fk_val = row_vals.get(child_col_idx as usize).unwrap_or(&Value::Null);
                    if matches!(fk_val, Value::Null) {
                        continue;
                    }
                    if let Ok(key) = crate::index_maintenance::fk_composite_key(fk_val, rid) {
                        BTree::insert_in(storage, &root_pid, &key, rid, 90)?;
                    }
                }

                let final_root = root_pid.load(Ordering::Acquire);
                let new_idx_id =
                    CatalogWriter::new(storage, txn, conn_txn)?.create_index(IndexDef {
                        index_id: 0,
                        table_id: child_table_id,
                        name: format!("_fk_{constraint_name}"),
                        root_page_id: final_root,
                        is_unique: false,
                        is_primary: false,
                        is_fk_index: true, // marks composite-key FK auto-index
                        columns: vec![CatIndexColumnDef {
                            col_idx: child_col_idx,
                            order: CatSortOrder::Asc,
                            expr: None,
                        }],
                        predicate: None,
                        fillfactor: 90,
                        include_columns: vec![],
                        index_type: 0,
                        pages_per_range: 128,
                    })?;
                new_idx_id
            }
        }
    };

    // 7. Persist FkDef in axiom_foreign_keys.
    CatalogWriter::new(storage, txn, conn_txn)?.create_foreign_key(FkDef {
        fk_id: 0, // allocated by CatalogWriter::create_foreign_key
        child_table_id,
        child_col_idx,
        parent_table_id: parent_def.id,
        parent_col_idx,
        on_delete,
        on_update,
        fk_index_id,
        name: constraint_name,
        child_col_idxs: vec![child_col_idx],
        parent_col_idxs: vec![parent_col_idx],
        deferrable: deferrability.deferrable,
        initially_deferred: matches!(
            deferrability.initially,
            crate::ast::ConstraintTiming::Deferred
        ),
    })?;

    Ok(())
}

// ── CREATE TABLE LIKE ─────────────────────────────────────────────────────────

/// Implements `CREATE TABLE new_table LIKE source_table`.
///
/// Copies the full schema (columns + indexes) from `source_table` into a new
/// empty table. No data is copied. FK constraints are intentionally not copied
/// (MySQL behaviour: `CREATE TABLE … LIKE` does not inherit FK constraints).
fn execute_create_table_like(
    stmt: crate::ast::CreateTableLikeStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    search_path: Option<&[String]>,
    database: &str,
) -> Result<QueryResult, DbError> {
    use axiomdb_index::page_layout::{cast_leaf_mut, NULL_PAGE};

    let new_schema = stmt.new_table.schema.as_deref().unwrap_or("public");
    let src_db = stmt.source_table.database.as_deref().unwrap_or(database);

    // 1. Resolve source table (read-only snapshot).
    let source = {
        let snap = txn.active_snapshot(conn_txn);
        if let Some(src_schema) = stmt.source_table.schema.as_deref() {
            let mut resolver = SchemaResolver::new(storage, snap, src_db, src_schema)?;
            resolver.resolve_table(Some(src_schema), &stmt.source_table.name)?
        } else if let Some(search_path) = search_path {
            let mut resolved = None;
            for schema in search_path {
                let mut resolver = SchemaResolver::new(storage, snap.clone(), src_db, schema)?;
                if let Ok(table) = resolver.resolve_table(Some(schema), &stmt.source_table.name) {
                    resolved = Some(table);
                    break;
                }
            }
            resolved.ok_or_else(|| DbError::TableNotFound {
                name: stmt.source_table.name.clone(),
            })?
        } else {
            let mut resolver = SchemaResolver::new(storage, snap, src_db, "public")?;
            resolver.resolve_table(Some("public"), &stmt.source_table.name)?
        }
    };

    // 2. Check the destination does not already exist.
    if stmt.persistence == axiomdb_catalog::TablePersistence::Temporary {
        ensure_schema_exists_for_create(storage, txn, conn_txn, database, new_schema)?;
    }
    {
        let mut resolver = make_resolver_with_database(storage, txn, Some(conn_txn), database)?;
        if resolver.table_exists(Some(new_schema), &stmt.new_table.name)? {
            if stmt.if_not_exists {
                return Ok(QueryResult::Empty);
            }
            return Err(DbError::TableAlreadyExists {
                schema: new_schema.to_string(),
                name: stmt.new_table.name.clone(),
            });
        }
    }

    // 3. Create the new table with the same storage layout.
    let new_def = {
        let mut writer = CatalogWriter::new(storage, txn, conn_txn)?;
        let def = writer.create_table_with_options(
            new_schema,
            &stmt.new_table.name,
            source.def.storage_layout,
            false,
            stmt.persistence,
            source.def.default_collation.clone(),
        )?;
        if database != DEFAULT_DATABASE_NAME {
            writer.bind_table_to_database(def.id, database)?;
        }
        def
    };
    let new_table_id = new_def.id;

    // 4. Copy columns (same col_idx, same type/nullable/auto_increment flags).
    for col in &source.columns {
        CatalogWriter::new(storage, txn, conn_txn)?.create_column(CatalogColumnDef {
            table_id: new_table_id,
            col_idx: col.col_idx,
            name: col.name.clone(),
            col_type: col.col_type,
            nullable: col.nullable,
            auto_increment: col.auto_increment,
            type_len: col.type_len,
            is_fixed_len: col.is_fixed_len,
            default_expr: col.default_expr.clone(),
            on_update_expr: col.on_update_expr.clone(),
            generated_expr: col.generated_expr.clone(),
            collation: col.collation.clone(),
            generated_stored: col.generated_stored,
            enum_type_name: col.enum_type_name.clone(),
            array_element_type: col.array_element_type,
            array_ndims: col.array_ndims,
        })?;
    }

    // 5. Copy indexes with fresh empty B-tree roots.
    //    For clustered tables the primary index root IS the table root page.
    let mut copied_index_ids = std::collections::HashMap::new();
    for idx in &source.indexes {
        let root_page_id = if idx.is_primary && source.def.is_clustered() {
            new_def.root_page_id
        } else {
            let pid = storage.alloc_page(PageType::Index)?;
            let mut page = Page::new(PageType::Index, pid);
            let leaf = cast_leaf_mut(&mut page);
            leaf.is_leaf = 1;
            leaf.set_num_keys(0);
            leaf.set_next_leaf(NULL_PAGE);
            page.update_checksum();
            storage.write_page(pid, &page)?;
            pid
        };

        let new_index_id = CatalogWriter::new(storage, txn, conn_txn)?.create_index(IndexDef {
            index_id: 0,
            table_id: new_table_id,
            name: idx.name.clone(),
            root_page_id,
            is_unique: idx.is_unique,
            is_primary: idx.is_primary,
            // FK-backing indexes are not preserved — FK constraints are not copied.
            is_fk_index: false,
            columns: idx.columns.clone(),
            predicate: idx.predicate.clone(),
            fillfactor: idx.fillfactor,
            include_columns: idx.include_columns.clone(),
            index_type: idx.index_type,
            pages_per_range: idx.pages_per_range,
        })?;
        copied_index_ids.insert(idx.index_id, new_index_id);
    }

    // 6. Copy CHECK / EXCLUDE constraints. FK constraints are intentionally
    // not copied (MySQL-compatible CREATE TABLE LIKE behavior).
    for constraint in &source.constraints {
        let owned_index_id = if constraint.kind == axiomdb_catalog::ConstraintKind::Exclusion {
            copied_index_ids.get(&constraint.owned_index_id).copied().ok_or_else(|| {
                DbError::Internal {
                    message: format!(
                        "missing copied helper index {} for exclusion constraint '{}'",
                        constraint.owned_index_id, constraint.name
                    ),
                }
            })?
        } else {
            0
        };

        CatalogWriter::new(storage, txn, conn_txn)?.create_constraint(
            axiomdb_catalog::schema::ConstraintDef {
                constraint_id: 0,
                table_id: new_table_id,
                name: constraint.name.clone(),
                check_expr: constraint.check_expr.clone(),
                kind: constraint.kind,
                owned_index_id,
                exclude_elements: constraint.exclude_elements.clone(),
            },
        )?;
    }

    Ok(QueryResult::Empty)
}

// ── CREATE TABLE / MATERIALIZED VIEW AS SELECT ───────────────────────────────

fn create_relation_as_select(
    if_not_exists: bool,
    new_table: crate::ast::TableRef,
    select: crate::ast::SelectStmt,
    persistence: axiomdb_catalog::TablePersistence,
    relation_kind: axiomdb_catalog::RelationKind,
    defining_query: Option<String>,
    exec_ctx: &ExecutionContext,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    // SAFETY: see ExecutionContext::storage_mut / coord_mut.
    let storage = exec_ctx.storage();
    let txn = exec_ctx.coord();
    let new_schema = new_table
        .schema
        .clone()
        .unwrap_or_else(|| ctx.default_create_schema().to_string());
    let database = ctx.effective_database().to_string();
    let new_name = new_table.name.clone();

    // 1. Run the SELECT (read-only — borrows storage/txn immutably via coercion).
    let conn = ctx.conn_txn.take();
    let result = execute_select_ctx(select, exec_ctx, conn.as_ref(), ctx)?;
    ctx.conn_txn = conn;
    let (col_meta, rows) = match result {
        QueryResult::Rows { columns, rows } => (columns, rows),
        // SELECT without FROM (e.g. SELECT 1) returns Rows with zero or one row.
        other => {
            return Err(DbError::Other(format!(
                "CTAS inner query returned unexpected result: {other:?}"
            )))
        }
    };

    // 2. Infer column types: first non-NULL value in each column determines the type.
    let num_cols = col_meta.len();
    let mut inferred: Vec<Option<ColumnType>> = vec![None; num_cols];
    'rows: for row in &rows {
        for (i, val) in row.iter().enumerate() {
            if inferred[i].is_some() {
                continue;
            }
            inferred[i] = match val {
                Value::Null => None,
                Value::Bool(_) => Some(ColumnType::Bool),
                Value::Int(_) => Some(ColumnType::Int),
                Value::BigInt(_) => Some(ColumnType::BigInt),
                Value::Real(_) => Some(ColumnType::Float),
                Value::Text(_) => Some(ColumnType::Text),
                Value::Bytes(_) => Some(ColumnType::Bytes),
                Value::Timestamp(_) => Some(ColumnType::Timestamp),
                Value::Uuid(_) => Some(ColumnType::Uuid),
                // Decimal / Date not in ColumnType yet — store as Text.
                _ => Some(ColumnType::Text),
            };
        }
        if inferred.iter().all(|t| t.is_some()) {
            break 'rows;
        }
    }
    let col_types: Vec<ColumnType> = inferred
        .into_iter()
        .map(|t| t.unwrap_or(ColumnType::Text))
        .collect();

    // 3. Check destination table does not already exist.
    if persistence == axiomdb_catalog::TablePersistence::Temporary {
        let conn_txn = ctx.conn_txn.as_mut().expect("conn_txn set for DDL");
        ensure_schema_exists_for_create(storage, txn, conn_txn, &database, &new_schema)?;
    }
    {
        let conn_txn = ctx.conn_txn.as_ref().expect("conn_txn set for DDL");
        let mut resolver = make_resolver_with_database(storage, txn, Some(conn_txn), &database)?;
        if resolver.table_exists(Some(&new_schema), &new_name)? {
            if if_not_exists {
                return Ok(QueryResult::Empty);
            }
            return Err(DbError::TableAlreadyExists {
                schema: new_schema.clone(),
                name: new_name.clone(),
            });
        }
    }

    // 4. Create the table (Heap — no primary key in CTAS).
    let new_def = {
        let conn_txn = ctx.conn_txn.as_mut().expect("conn_txn set for DDL");
        let mut writer = CatalogWriter::new(storage, txn, conn_txn)?;
        let def = writer.create_relation_with_options(
            &new_schema,
            &new_name,
            axiomdb_catalog::schema::TableStorageLayout::Heap,
            false,
            persistence,
            relation_kind,
            defining_query,
            None,
        )?;
        if database != DEFAULT_DATABASE_NAME {
            writer.bind_table_to_database(def.id, &database)?;
        }
        def
    };

    // 5. Create columns from inferred types + output column names.
    for (i, (meta, col_type)) in col_meta.iter().zip(col_types.iter()).enumerate() {
        let conn_txn = ctx.conn_txn.as_mut().expect("conn_txn set for DDL");
        CatalogWriter::new(storage, txn, conn_txn)?.create_column(CatalogColumnDef {
            table_id: new_def.id,
            col_idx: i as u16,
            name: meta.name.clone(),
            col_type: *col_type,
            nullable: true, // CTAS columns are always nullable
            auto_increment: false,
            type_len: 0,
            is_fixed_len: false,
            default_expr: None,
            on_update_expr: None,
            generated_expr: None,
            collation: None,
            generated_stored: false,
                enum_type_name: None,
                array_element_type: None,
                array_ndims: None,
        })?;
    }

    // Build a ColumnDef slice for TableEngine::insert_row.
    let schema_cols: Vec<CatalogColumnDef> = col_meta
        .iter()
        .zip(col_types.iter())
        .enumerate()
        .map(|(i, (meta, &col_type))| CatalogColumnDef {
            table_id: new_def.id,
            col_idx: i as u16,
            name: meta.name.clone(),
            col_type,
            nullable: true,
            auto_increment: false,
            type_len: 0,
            is_fixed_len: false,
            default_expr: None,
            on_update_expr: None,
            generated_expr: None,
            collation: None,
            generated_stored: false,
                enum_type_name: None,
                array_element_type: None,
                array_ndims: None,
        })
        .collect();

    // 6. Insert all rows into the new table.
    for row in rows {
        let conn_txn = ctx.conn_txn.as_mut().expect("conn_txn set for DDL");
        TableEngine::insert_row(storage, txn, conn_txn, &new_def, &schema_cols, row)?;
    }

    Ok(QueryResult::Empty)
}

/// Implements `CREATE TABLE new_table AS SELECT …`.
fn execute_create_table_as_select(
    stmt: crate::ast::CreateTableAsSelectStmt,
    exec_ctx: &ExecutionContext,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    create_relation_as_select(
        false,
        stmt.new_table,
        stmt.select,
        stmt.persistence,
        axiomdb_catalog::RelationKind::Table,
        None,
        exec_ctx,
        ctx,
    )
}

fn execute_create_materialized_view(
    stmt: crate::ast::CreateMaterializedViewStmt,
    exec_ctx: &ExecutionContext,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let mut view = stmt.view;
    if view.schema.is_none() {
        view.schema = Some(ctx.default_create_schema().to_string());
    }
    create_relation_as_select(
        stmt.if_not_exists,
        view,
        stmt.select,
        axiomdb_catalog::TablePersistence::Permanent,
        axiomdb_catalog::RelationKind::MaterializedView,
        Some(stmt.query_sql),
        exec_ctx,
        ctx,
    )
}

fn execute_refresh_materialized_view(
    stmt: crate::ast::RefreshMaterializedViewStmt,
    exec_ctx: &ExecutionContext,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let storage = exec_ctx.storage();
    let txn = exec_ctx.coord();
    let database = stmt
        .view
        .database
        .clone()
        .unwrap_or_else(|| ctx.effective_database().to_string());
    let resolved = {
        let conn_txn = ctx.conn_txn.as_mut().expect("conn_txn set for DDL");
        let mut resolver = make_resolver_with_database(storage, txn, Some(conn_txn), &database)?;
        resolver.resolve_table(stmt.view.schema.as_deref(), &stmt.view.name)?
    };
    if !resolved.def.is_materialized_view() {
        return Err(DbError::InvalidValue {
            reason: format!("'{}' is not a materialized view", stmt.view.name),
        });
    }
    let query_sql = resolved
        .def
        .defining_query
        .clone()
        .ok_or_else(|| DbError::Internal {
            message: format!(
                "materialized view '{}' is missing its defining query",
                resolved.def.table_name
            ),
        })?;

    let conn = ctx.conn_txn.take();
    let snapshot = conn
        .as_ref()
        .map(|txn_conn| txn.active_snapshot(txn_conn))
        .unwrap_or_else(|| txn.snapshot());
    let parsed = crate::parse(&query_sql, None)?;
    let analyzed =
        crate::analyze_with_defaults(parsed, storage, snapshot, &database, &resolved.def.schema_name)?;
    let select = match analyzed {
        crate::ast::Stmt::Select(select) => select,
        other => {
            return Err(DbError::InvalidValue {
                reason: format!(
                    "materialized view '{}' defining query must be a SELECT, found {other:?}",
                    resolved.def.table_name
                ),
            })
        }
    };
    let result = execute_select_ctx(select, exec_ctx, conn.as_ref(), ctx)?;
    ctx.conn_txn = conn;
    let rows = match result {
        QueryResult::Rows { columns, rows } => {
            if columns.len() != resolved.columns.len() {
                return Err(DbError::InvalidValue {
                    reason: format!(
                        "materialized view '{}' refresh produced {} columns but expected {}",
                        resolved.def.table_name,
                        columns.len(),
                        resolved.columns.len()
                    ),
                });
            }
            rows
        }
        other => {
            return Err(DbError::Other(format!(
                "materialized view refresh query returned unexpected result: {other:?}"
            )))
        }
    };

    let truncate_stmt = crate::ast::TruncateTableStmt {
        table: crate::ast::TableRef {
            database: None,
            schema: Some(resolved.def.schema_name.clone()),
            name: resolved.def.table_name.clone(),
            alias: None,
            tablesample: None,
        },
    };
    let conn_txn = ctx.conn_txn.as_mut().expect("conn_txn set for DDL");
    execute_truncate(truncate_stmt, storage, txn, conn_txn, &database)?;
    let refreshed = {
        let conn_txn = ctx.conn_txn.as_mut().expect("conn_txn set for DDL");
        let mut resolver = make_resolver_with_database(storage, txn, Some(conn_txn), &database)?;
        resolver.resolve_table(Some(&resolved.def.schema_name), &resolved.def.table_name)?
    };
    for row in rows {
        let conn_txn = ctx.conn_txn.as_mut().expect("conn_txn set for DDL");
        TableEngine::insert_row(
            storage,
            txn,
            conn_txn,
            &refreshed.def,
            &refreshed.columns,
            row,
        )?;
    }

    Ok(QueryResult::Empty)
}

// ── Composite foreign key (GAP-C.2) ──────────────────────────────────────────

/// Persists a multi-column FK. Requires that both parent and child already
/// have an index whose leading columns exactly match the FK column list
/// (parent: PRIMARY KEY or UNIQUE covering `parent_col_idxs`; child: any
/// index whose prefix matches `child_col_idxs`). This avoids the complexity
/// of auto-building a composite child index on existing rows — a tradeoff
/// that mirrors MySQL's recommendation to always pre-declare the child
/// composite index explicitly.
#[allow(clippy::too_many_arguments)]
fn persist_composite_fk_constraint(
    child_table_id: u32,
    child_table_name: &str,
    database: &str,
    child_col_idxs: &[u16],
    child_col_names: &[String],
    ref_table: &str,
    ref_columns: &[String],
    on_delete: axiomdb_catalog::FkAction,
    on_update: axiomdb_catalog::FkAction,
    deferrability: crate::ast::ConstraintDeferrability,
    fk_name: Option<&str>,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
) -> Result<(), DbError> {
    use axiomdb_catalog::FkDef;

    let snap = txn.active_snapshot(conn_txn);

    let child_table_def = {
        let mut reader = CatalogReader::new(storage, snap.clone())?;
        reader
            .get_table_by_id(child_table_id)?
            .ok_or(DbError::CatalogTableNotFound {
                table_id: child_table_id,
            })?
    };
    reject_fk_persistence(child_table_def.persistence, false)?;

    // 1. Resolve parent table + columns.
    let parent_def =
        resolve_fk_parent_table(database, ref_table, &child_table_def.schema_name, storage, snap.clone())?;
    reject_fk_persistence(parent_def.persistence, true)?;
    let parent_cols = {
        let mut reader = CatalogReader::new(storage, snap.clone())?;
        reader.list_columns(parent_def.id)?
    };

    // Parent column list: must match arity. If REFERENCES specifies columns
    // explicitly, use those; otherwise default to the PK leading columns.
    let parent_col_idxs: Vec<u16> = if ref_columns.is_empty() {
        let parent_indexes = {
            let mut reader = CatalogReader::new(storage, snap.clone())?;
            reader.list_indexes(parent_def.id)?
        };
        let pk = parent_indexes
            .iter()
            .find(|i| i.is_primary && i.columns.len() >= child_col_idxs.len())
            .ok_or_else(|| DbError::ForeignKeyNoParentIndex {
                table: ref_table.to_string(),
                column: "<primary key>".to_string(),
            })?;
        pk.columns
            .iter()
            .take(child_col_idxs.len())
            .map(|c| c.col_idx)
            .collect()
    } else {
        if ref_columns.len() != child_col_idxs.len() {
            return Err(DbError::InvalidValue {
                reason: format!(
                    "composite FK arity mismatch: child has {} column(s), REFERENCES has {}",
                    child_col_idxs.len(),
                    ref_columns.len()
                ),
            });
        }
        ref_columns
            .iter()
            .map(|name| {
                parent_cols
                    .iter()
                    .find(|c| &c.name == name)
                    .map(|c| c.col_idx)
                    .ok_or_else(|| DbError::ColumnNotFound {
                        name: name.clone(),
                        table: ref_table.to_string(),
                    })
            })
            .collect::<Result<_, _>>()?
    };

    // 2. Verify parent has a PK or UNIQUE index covering every parent col in order.
    {
        let mut reader = CatalogReader::new(storage, snap.clone())?;
        let parent_indexes = reader.list_indexes(parent_def.id)?;
        let covered = parent_indexes.iter().any(|i| {
            (i.is_primary || i.is_unique)
                && i.columns.len() >= parent_col_idxs.len()
                && i.columns
                    .iter()
                    .take(parent_col_idxs.len())
                    .zip(parent_col_idxs.iter())
                    .all(|(ic, wanted)| ic.col_idx == *wanted)
        });
        if !covered {
            return Err(DbError::ForeignKeyNoParentIndex {
                table: ref_table.to_string(),
                column: format!("({})", ref_columns.join(",")),
            });
        }
    }

    // 3. Auto-generate FK name if not provided.
    let constraint_name: String = fk_name.map(|n| n.to_string()).unwrap_or_else(|| {
        format!(
            "fk_{child_table_name}_{}_{ref_table}",
            child_col_names.join("_")
        )
    });

    // 4. Name uniqueness on this child table.
    {
        let mut reader = CatalogReader::new(storage, snap.clone())?;
        if reader
            .get_fk_by_name(child_table_id, &constraint_name)?
            .is_some()
        {
            return Err(DbError::Other(format!(
                "foreign key constraint '{constraint_name}' already exists on table \
                 '{child_table_name}'"
            )));
        }
    }

    // 5. Verify child already has an index whose leading columns match
    //    `child_col_idxs`. Auto-building a composite FK index on existing
    //    child rows is not yet supported — user must declare it explicitly.
    let child_has_index = {
        let mut reader = CatalogReader::new(storage, snap.clone())?;
        reader.list_indexes(child_table_id)?.into_iter().any(|i| {
            !i.is_fk_index
                && i.columns.len() >= child_col_idxs.len()
                && i.columns
                    .iter()
                    .take(child_col_idxs.len())
                    .zip(child_col_idxs.iter())
                    .all(|(ic, wanted)| ic.col_idx == *wanted)
        })
    };
    if !child_has_index {
        return Err(DbError::InvalidValue {
            reason: format!(
                "composite FK '{constraint_name}' requires a pre-declared index on \
                 child columns ({}); auto-creation of composite FK indexes is not \
                 supported yet",
                child_col_names.join(",")
            ),
        });
    }

    // 6. Persist FkDef with composite vectors. `fk_index_id = 0` → user-provided.
    CatalogWriter::new(storage, txn, conn_txn)?.create_foreign_key(FkDef {
        fk_id: 0,
        child_table_id,
        child_col_idx: child_col_idxs[0],
        parent_table_id: parent_def.id,
        parent_col_idx: parent_col_idxs[0],
        on_delete,
        on_update,
        fk_index_id: 0,
        name: constraint_name,
        child_col_idxs: child_col_idxs.to_vec(),
        parent_col_idxs,
        deferrable: deferrability.deferrable,
        initially_deferred: matches!(
            deferrability.initially,
            crate::ast::ConstraintTiming::Deferred
        ),
    })?;

    Ok(())
}
