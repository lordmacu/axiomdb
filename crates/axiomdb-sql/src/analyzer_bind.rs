// ── Public entry point ────────────────────────────────────────────────────────

/// Semantic analysis: validate all references and resolve `col_idx`.
///
/// Takes a parsed `Stmt` (all `col_idx = 0`) and returns the same `Stmt`
/// with every `Expr::Column.col_idx` set to its correct position in the
/// combined row produced by the FROM clause.
///
/// # Errors
/// - [`DbError::TableNotFound`]    — unknown table or alias
/// - [`DbError::ColumnNotFound`]   — column not in any in-scope table
/// - [`DbError::AmbiguousColumn`]  — unqualified name in multiple tables
pub fn analyze(
    stmt: Stmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
) -> Result<Stmt, DbError> {
    analyze_with_defaults(stmt, storage, snapshot, DEFAULT_DATABASE_NAME, "public")
}

/// Cached variant of [`analyze`] — skips catalog heap scans for tables already
/// seen in this session.
///
/// Pass the same `cache` across repeated calls to the same tables (e.g. bulk
/// INSERT loops). The caller is responsible for calling [`SchemaCache::invalidate`]
/// after any DDL statement that changes the schema.
///
/// First call per table: normal catalog scan + populates cache.
/// Subsequent calls: cache hit, zero catalog I/O.
pub fn analyze_cached(
    stmt: Stmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    cache: &mut SchemaCache,
) -> Result<Stmt, DbError> {
    analyze_cached_with_defaults(
        stmt,
        storage,
        snapshot,
        DEFAULT_DATABASE_NAME,
        "public",
        cache,
    )
}

pub fn analyze_with_defaults(
    stmt: Stmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
) -> Result<Stmt, DbError> {
    analyze_stmt(stmt, storage, snapshot, default_database, default_schema)
}

pub fn analyze_cached_with_defaults(
    stmt: Stmt,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
    cache: &mut SchemaCache,
) -> Result<Stmt, DbError> {
    analyze_stmt_cached(
        stmt,
        storage,
        snapshot,
        default_database,
        default_schema,
        cache,
    )
}

// ── BindContext ───────────────────────────────────────────────────────────────

/// Resolution context built from the FROM and JOIN clauses of a SELECT.
struct BindContext {
    tables: Vec<BoundTable>,
}

struct BoundTable {
    /// Alias from `FROM users AS u`, or `None` if no alias.
    alias: Option<String>,
    /// Real table name in the catalog.
    name: String,
    /// Columns in declaration order (from `CatalogReader::list_columns`).
    columns: Vec<ColumnDef>,
    /// Start position of this table's columns in the combined row.
    col_offset: usize,
}

impl BindContext {
    fn empty() -> Self {
        Self { tables: vec![] }
    }

    /// Find a table by its alias or name.
    fn find_table(&self, qualifier: &str) -> Option<&BoundTable> {
        self.tables
            .iter()
            .find(|t| t.alias.as_deref() == Some(qualifier) || t.name == qualifier)
    }

    /// Find all (table_idx, col_idx_global, col_name) for an unqualified column.
    fn find_column_all(&self, col_name: &str) -> Vec<(usize, usize, &str)> {
        let mut found = Vec::new();
        for (ti, table) in self.tables.iter().enumerate() {
            for (ci, col) in table.columns.iter().enumerate() {
                if col.name.eq_ignore_ascii_case(col_name) {
                    found.push((ti, table.col_offset + ci, col.name.as_str()));
                }
            }
        }
        found
    }

    /// Resolve a qualified `table.column` or unqualified `column` reference.
    fn resolve_column(&self, name: &str) -> Result<usize, DbError> {
        self.resolve_column_with_def(name).map(|(idx, _)| idx)
    }

    fn resolve_column_with_def(&self, name: &str) -> Result<(usize, &ColumnDef), DbError> {
        let (qualifier, field) = split_name(name);

        if let Some(q) = qualifier {
            // Qualified: find the specific table
            let table = self.find_table(q).ok_or_else(|| DbError::TableNotFound {
                name: format!("{q}.{field}"),
            })?;
            let (pos, col) = find_col_in_table(table, field, q)?;
            Ok((table.col_offset + pos, col))
        } else {
            // Unqualified: search all tables
            let matches = self.find_column_all(field);
            match matches.len() {
                0 => {
                    let available = self
                        .tables
                        .iter()
                        .flat_map(|t| t.columns.iter().map(|c| c.name.as_str()))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let table_names = self
                        .tables
                        .iter()
                        .map(|t| t.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    Err(DbError::ColumnNotFound {
                        name: field.to_string(),
                        table: format!("\"{}\" (available: {})", table_names, available),
                    })
                }
                1 => {
                    let (ti, idx, _) = matches[0];
                    let table = &self.tables[ti];
                    let local_idx = idx - table.col_offset;
                    Ok((idx, &table.columns[local_idx]))
                }
                _ => {
                    let table_list = matches
                        .iter()
                        .map(|(ti, _, _)| self.tables[*ti].name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    Err(DbError::AmbiguousColumn {
                        name: field.to_string(),
                        tables: table_list,
                    })
                }
            }
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Split "table.column" into ("table", "column") or (None, "column").
fn split_name(name: &str) -> (Option<&str>, &str) {
    match name.find('.') {
        Some(i) => (Some(&name[..i]), &name[i + 1..]),
        None => (None, name),
    }
}

/// Find a column by name within a single BoundTable.
fn find_col_in_table<'a>(
    table: &'a BoundTable,
    col_name: &str,
    context: &str,
) -> Result<(usize, &'a ColumnDef), DbError> {
    table
        .columns
        .iter()
        .enumerate()
        .find(|(_, c)| c.name.eq_ignore_ascii_case(col_name))
        .ok_or_else(|| {
            let available = table
                .columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            DbError::ColumnNotFound {
                name: col_name.to_string(),
                table: format!("\"{context}\" (available: {available})"),
            }
        })
}

fn effective_bound_collation(
    column: &ColumnDef,
    table_default_collation: Option<&str>,
    database_default_collation: Option<&str>,
) -> Option<String> {
    match column.col_type {
        axiomdb_catalog::ColumnType::Text => column
            .collation
            .clone()
            .or_else(|| table_default_collation.map(ToOwned::to_owned))
            .or_else(|| database_default_collation.map(ToOwned::to_owned)),
        _ => None,
    }
}

/// Levenshtein distance — O(|a|*|b|), both strings ≤ 64 chars (Phase 4.3d).
/// Used for typo hints; kept for Phase 4.18b typo suggestion feature.
#[allow(dead_code)]
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (m, n) = (a.len(), b.len());
    let mut dp = vec![vec![0usize; n + 1]; m + 1];
    (0..=m).for_each(|i| dp[i][0] = i);
    (0..=n).for_each(|j| dp[0][j] = j);
    for i in 1..=m {
        for j in 1..=n {
            dp[i][j] = if a[i - 1] == b[j - 1] {
                dp[i - 1][j - 1]
            } else {
                1 + dp[i - 1][j].min(dp[i][j - 1]).min(dp[i - 1][j - 1])
            };
        }
    }
    dp[m][n]
}

// ── BindContext construction ──────────────────────────────────────────────────

fn build_context(
    from: &Option<FromClause>,
    joins: &[crate::ast::JoinClause],
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
    outer_scopes: &[&BindContext],
) -> Result<BindContext, DbError> {
    let mut ctx = BindContext::empty();
    let mut col_offset = 0usize;

    if let Some(from_clause) = from {
        let bound = bound_from_clause(
            from_clause,
            storage,
            snapshot.clone(),
            default_database,
            default_schema,
            &mut col_offset,
            outer_scopes,
        )?;
        ctx.tables.extend(bound);
    }

    // For each join, pass the accumulated context as an additional outer scope so that
    // LATERAL subqueries can reference tables from the left side of the join.
    for join in joins {
        // Construct extended outer scopes: current accumulated ctx becomes an additional
        // outer scope (depth increases by 1 for all previous tables).
        let mut extended_scopes: Vec<&BindContext> = outer_scopes.to_vec();
        extended_scopes.push(&ctx);
        let bound = bound_from_clause(
            &join.table,
            storage,
            snapshot.clone(),
            default_database,
            default_schema,
            &mut col_offset,
            &extended_scopes,
        )?;
        ctx.tables.extend(bound);
    }

    Ok(ctx)
}

fn bound_from_clause(
    from: &FromClause,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
    col_offset: &mut usize,
    outer_scopes: &[&BindContext],
) -> Result<Vec<BoundTable>, DbError> {
    match from {
        FromClause::Table(table_ref) => {
            let bound = bound_table_ref(
                table_ref,
                storage,
                snapshot,
                default_database,
                default_schema,
                col_offset,
            )?;
            Ok(vec![bound])
        }
        FromClause::Subquery {
            query,
            alias,
            lateral,
        } => {
            // Phase 21.9: For LATERAL subqueries, analyze with outer scopes so that
            // correlated references (e.g., `t.id` in `FROM t, LATERAL (SELECT t.id ...)`)
            // are resolved as OuterColumn. For non-LATERAL, use analyze_select since
            // outer correlation is not allowed.
            let analyzed_query = if *lateral {
                analyze_select_with_outer(
                    *query.clone(),
                    storage,
                    snapshot,
                    default_database,
                    default_schema,
                    outer_scopes,
                )?
            } else {
                analyze_select(
                    *query.clone(),
                    storage,
                    snapshot,
                    default_database,
                    default_schema,
                )?
            };
            let virtual_cols = virtual_columns_from_select(&analyzed_query);
            let n = virtual_cols.len();
            let bound = BoundTable {
                alias: Some(alias.clone()),
                name: alias.clone(),
                columns: virtual_cols,
                col_offset: *col_offset,
            };
            *col_offset += n;
            Ok(vec![bound])
        }
        FromClause::Pivot(pivot) => {
            let mut local_col_offset = 0usize;
            let source_bounds = bound_from_clause(
                &pivot.source,
                storage,
                snapshot.clone(),
                default_database,
                default_schema,
                &mut local_col_offset,
                outer_scopes,
            )?;
            if source_bounds.len() != 1 {
                return Err(DbError::Internal {
                    message: "PIVOT source must bind to exactly one table shape".into(),
                });
            }
            let source_ctx = BindContext {
                tables: source_bounds,
            };
            let state = AnalyzeState {
                storage,
                snapshot,
                default_database,
                default_schema,
            };
            let resolved_pivot_expr = resolve_expr_full(
                pivot.pivot_expr.clone(),
                &source_ctx,
                outer_scopes,
                Some(&state),
            )?;
            let resolved_aggregate_arg = resolve_expr_full(
                pivot.aggregate_arg.clone(),
                &source_ctx,
                outer_scopes,
                Some(&state),
            )?;
            let virtual_cols = build_pivot_virtual_columns(
                pivot,
                &source_ctx.tables[0].columns,
                &resolved_pivot_expr,
                &resolved_aggregate_arg,
            )?;
            let n = virtual_cols.len();
            let alias = pivot_result_alias(pivot);
            let bound = BoundTable {
                alias: Some(alias.clone()),
                name: alias,
                columns: virtual_cols,
                col_offset: *col_offset,
            };
            *col_offset += n;
            Ok(vec![bound])
        }
        // Phase 11.20a — JSON_TABLE: publish the COLUMNS(...) declarations
        // as a virtual BoundTable. The `doc` expression is resolved later in
        // `analyze_select_with_outer` against the accumulated BindContext.
        FromClause::JsonTable(jt) => {
            let virtual_cols = crate::json_table::column_defs_for_ast(jt)?;
            let n = virtual_cols.len();
            let alias = jt.alias.clone().unwrap_or_else(|| "json_table".into());
            let bound = BoundTable {
                alias: Some(alias.clone()),
                name: alias,
                columns: virtual_cols,
                col_offset: *col_offset,
            };
            *col_offset += n;
            Ok(vec![bound])
        }
        // Phase 11.25a — JSONB SRF virtual table (jsonb_each, etc.).
        FromClause::JsonbSrf(srf) => {
            let virtual_cols = crate::jsonb_srf::column_defs_for_srf(srf.kind);
            let n = virtual_cols.len();
            let alias = crate::jsonb_srf::srf_alias(srf);
            let bound = BoundTable {
                alias: Some(alias.clone()),
                name: alias,
                columns: virtual_cols,
                col_offset: *col_offset,
            };
            *col_offset += n;
            Ok(vec![bound])
        }
        // Phase 21.22 — inline VALUES virtual table.
        FromClause::Values(vc) => {
            let virtual_cols = crate::values_clause::column_defs_for_values(vc);
            let n = virtual_cols.len();
            let alias = vc.alias.clone();
            let bound = BoundTable {
                alias: Some(alias.clone()),
                name: alias,
                columns: virtual_cols,
                col_offset: *col_offset,
            };
            *col_offset += n;
            Ok(vec![bound])
        }
        // Phase 21.3 — recursive CTE virtual table. Columns inherited
        // from the base SELECT's analyzed output.
        FromClause::RecursiveCte(rc) => {
            let virtual_cols = crate::recursive_cte::column_defs_for_recursive(rc);
            let n = virtual_cols.len();
            let alias = rc.alias.clone();
            let bound = BoundTable {
                alias: Some(alias.clone()),
                name: alias,
                columns: virtual_cols,
                col_offset: *col_offset,
            };
            *col_offset += n;
            Ok(vec![bound])
        }
        // Phase 20.4, Step 7 — UNNEST virtual table.
        FromClause::Unnest(un) => {
            let virtual_cols = crate::unnest::column_defs_for_unnest(un);
            let n = virtual_cols.len();
            let alias = un.alias.clone().unwrap_or_else(|| "unnest".into());
            let bound = BoundTable {
                alias: Some(alias.clone()),
                name: alias,
                columns: virtual_cols,
                col_offset: *col_offset,
            };
            *col_offset += n;
            Ok(vec![bound])
        }
        // Phase 20.10 — GENERATE_SERIES virtual table.
        FromClause::GenerateSeries(gs) => {
            let virtual_cols = crate::generate_series::column_defs_for_generate_series(gs);
            let n = virtual_cols.len();
            let alias = gs
                .alias
                .clone()
                .unwrap_or_else(|| "generate_series".into());
            let bound = BoundTable {
                alias: Some(alias.clone()),
                name: alias,
                columns: virtual_cols,
                col_offset: *col_offset,
            };
            *col_offset += n;
            Ok(vec![bound])
        }
        // Phase 20.6 — READ_PARQUET('path') TVF: schema discovery at bind time.
        FromClause::ReadParquet(rp) => {
            let cols = parquet_schema_to_column_defs(&rp.path, &rp.column_aliases)?;
            let n = cols.len();
            let alias = rp.alias.clone().unwrap_or_else(|| "read_parquet".into());
            let bound = BoundTable {
                alias: Some(alias.clone()),
                name: alias,
                columns: cols,
                col_offset: *col_offset,
            };
            *col_offset += n;
            Ok(vec![bound])
        }
        // Phase 20.20 — XMLTABLE virtual table.
        FromClause::XmlTable(xt) => {
            let spec = crate::xml_table::compile_xml_table(xt)?;
            let metas = crate::xml_table::column_metas_for_spec(&spec);
            let virtual_cols: Vec<axiomdb_catalog::schema::ColumnDef> = metas
                .iter()
                .enumerate()
                .map(|(i, m)| {
                    let col_type = crate::json_table::datatype_to_column_type_pub(&m.data_type)
                        .unwrap_or(axiomdb_catalog::schema::ColumnType::Text);
                    axiomdb_catalog::schema::ColumnDef {
                        table_id: 0,
                        col_idx: i as u16,
                        name: m.name.clone(),
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
                    }
                })
                .collect();
            let n = virtual_cols.len();
            let alias = xt.alias.clone().unwrap_or_else(|| "xmltable".into());
            let bound = BoundTable {
                alias: Some(alias.clone()),
                name: alias,
                columns: virtual_cols,
                col_offset: *col_offset,
            };
            *col_offset += n;
            Ok(vec![bound])
        }
    }
}

/// Open a Parquet file, read its schema, and produce a `ColumnDef` list.
/// Called at bind time so the analyzer knows column names + types before execution.
fn parquet_schema_to_column_defs(
    path: &str,
    column_aliases: &[String],
) -> Result<Vec<axiomdb_catalog::schema::ColumnDef>, DbError> {
    use axiomdb_catalog::schema::{ColumnDef, TableId};
    use parquet::basic::Repetition;
    use parquet::file::reader::{FileReader, SerializedFileReader};
    use parquet::schema::types::Type;

    let file = std::fs::File::open(path).map_err(|e| {
        DbError::Io(std::io::Error::new(
            e.kind(),
            format!("READ_PARQUET: cannot open '{path}': {e}"),
        ))
    })?;
    let reader = SerializedFileReader::new(file).map_err(|e| DbError::InvalidValue {
        reason: format!("READ_PARQUET: cannot read Parquet metadata from '{path}': {e}"),
    })?;
    let schema = reader.metadata().file_metadata().schema_descr_ptr();
    let root = schema.root_schema();
    let fields = match root {
        Type::GroupType { fields, .. } => fields,
        Type::PrimitiveType { .. } => {
            return Err(DbError::InvalidValue {
                reason: format!("READ_PARQUET: unexpected non-group root schema in '{path}'"),
            });
        }
    };

    if !column_aliases.is_empty() && column_aliases.len() != fields.len() {
        return Err(DbError::InvalidValue {
            reason: format!(
                "READ_PARQUET column alias count ({}) does not match Parquet schema column count ({})",
                column_aliases.len(),
                fields.len()
            ),
        });
    }

    let mut cols = Vec::with_capacity(fields.len());
    for (i, field) in fields.iter().enumerate() {
        let name = if column_aliases.is_empty() {
            field.name().to_string()
        } else {
            column_aliases[i].clone()
        };
        let col_type = match field.as_ref() {
            Type::PrimitiveType {
                basic_info,
                physical_type,
                ..
            } => {
                // Reject REPEATED fields (nested types not supported).
                if basic_info.repetition() == Repetition::REPEATED {
                    return Err(DbError::NotImplemented {
                        feature: "Parquet nested types (REPEATED fields)".into(),
                    });
                }
                let logical = basic_info.logical_type();
                let converted = basic_info.converted_type();
                parquet_type_to_column_type(*physical_type, logical.as_ref(), converted)?
            }
            Type::GroupType { basic_info, .. } => {
                // Nested group types (LIST, MAP, STRUCT) not supported.
                if basic_info.repetition() == Repetition::REPEATED {
                    return Err(DbError::NotImplemented {
                        feature: "Parquet nested types (group fields)".into(),
                    });
                }
                return Err(DbError::NotImplemented {
                    feature: "Parquet nested group types (LIST/MAP/STRUCT)".into(),
                });
            }
        };
        cols.push(ColumnDef {
            table_id: 0 as TableId,
            col_idx: i as u16,
            name,
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
        });
    }
    Ok(cols)
}

fn parquet_type_to_column_type(
    physical: parquet::basic::Type,
    logical: Option<&parquet::basic::LogicalType>,
    converted: parquet::basic::ConvertedType,
) -> Result<axiomdb_catalog::schema::ColumnType, DbError> {
    use axiomdb_catalog::schema::ColumnType;
    use parquet::basic::{ConvertedType, LogicalType, Type as Physical};

    if let Some(lt) = logical {
        match lt {
            LogicalType::Date => return Ok(ColumnType::Date),
            LogicalType::Timestamp { .. } => return Ok(ColumnType::Timestamp),
            LogicalType::String | LogicalType::Enum | LogicalType::Uuid => {
                return Ok(ColumnType::Text)
            }
            LogicalType::Decimal { .. } => return Ok(ColumnType::Decimal),
            LogicalType::Integer { .. } => {
                return Ok(match physical {
                    Physical::INT32 => ColumnType::Int,
                    _ => ColumnType::BigInt,
                });
            }
            LogicalType::Json => return Ok(ColumnType::Json),
            LogicalType::Bson => return Ok(ColumnType::Bytes),
            _ => {}
        }
    }

    // Fall back to converted type (legacy Parquet files).
    match converted {
        ConvertedType::DATE => return Ok(ColumnType::Date),
        ConvertedType::TIMESTAMP_MICROS | ConvertedType::TIMESTAMP_MILLIS => {
            return Ok(ColumnType::Timestamp)
        }
        ConvertedType::UTF8 | ConvertedType::ENUM => return Ok(ColumnType::Text),
        ConvertedType::DECIMAL => return Ok(ColumnType::Decimal),
        ConvertedType::INT_8
        | ConvertedType::INT_16
        | ConvertedType::INT_32
        | ConvertedType::UINT_8
        | ConvertedType::UINT_16
        | ConvertedType::UINT_32 => return Ok(ColumnType::Int),
        ConvertedType::INT_64 | ConvertedType::UINT_64 => return Ok(ColumnType::BigInt),
        ConvertedType::JSON => return Ok(ColumnType::Json),
        ConvertedType::BSON => return Ok(ColumnType::Bytes),
        _ => {}
    }

    // Physical type fallback (INT96 is a deprecated Impala timestamp type).
    Ok(match physical {
        Physical::BOOLEAN => ColumnType::Bool,
        Physical::INT32 => ColumnType::Int,
        Physical::INT64 => ColumnType::BigInt,
        Physical::INT96 => ColumnType::Timestamp,
        Physical::FLOAT | Physical::DOUBLE => ColumnType::Float,
        Physical::BYTE_ARRAY | Physical::FIXED_LEN_BYTE_ARRAY => ColumnType::Text,
    })
}

fn bound_table_ref(
    table_ref: &TableRef,
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
    default_database: &str,
    default_schema: &str,
    col_offset: &mut usize,
) -> Result<BoundTable, DbError> {
    let database = table_ref.database.as_deref().unwrap_or(default_database);
    let schema = table_ref.schema.as_deref().unwrap_or(default_schema);

    // INFORMATION_SCHEMA virtual tables (4.20c) — synthetic column binding,
    // no catalog lookup required.
    if crate::information_schema::is_information_schema(schema) {
        let cols = crate::information_schema::make_is_catalog_columns(&table_ref.name, *col_offset)
            .ok_or_else(|| DbError::TableNotFound {
                name: format!("information_schema.{}", table_ref.name),
            })?;
        let n = cols.len();
        let bound = BoundTable {
            alias: table_ref.alias.clone(),
            name: table_ref.name.clone(),
            columns: cols,
            col_offset: *col_offset,
        };
        *col_offset += n;
        return Ok(bound);
    }

    let mut reader = CatalogReader::new(storage, snapshot)?;

    // If the user explicitly specified a database, verify it exists first.
    if table_ref.database.is_some() && !reader.database_exists(database)? {
        return Err(DbError::DatabaseNotFound {
            name: database.to_string(),
        });
    }

    // When the user didn't specify an explicit schema, try the default schema
    // first, then fall back to "public" if different (search_path fallback).
    let table_def = if table_ref.schema.is_none() {
        let mut found = reader.get_table_in_database(database, schema, &table_ref.name)?;
        if found.is_none() && schema != "public" {
            found = reader.get_table_in_database(database, "public", &table_ref.name)?;
        }
        found
    } else {
        reader.get_table_in_database(database, schema, &table_ref.name)?
    };

    if let Some(table_def) = table_def {
        let database_default_collation = reader
            .get_database(database)?
            .and_then(|db| db.default_collation);
        let columns = reader
            .list_columns(table_def.id)?
            .into_iter()
            .map(|mut col| {
                col.collation = effective_bound_collation(
                    &col,
                    table_def.default_collation.as_deref(),
                    database_default_collation.as_deref(),
                );
                col
            })
            .collect::<Vec<_>>();
        let n = columns.len();
        let bound = BoundTable {
            alias: table_ref.alias.clone(),
            name: table_ref.name.clone(),
            columns,
            col_offset: *col_offset,
        };
        *col_offset += n;
        return Ok(bound);
    }

    // Regular table not found — try foreign table catalog (FDW, Phase 22b.2).
    let ftable = if table_ref.schema.is_none() {
        let mut found = reader.get_foreign_table(schema, &table_ref.name)?;
        if found.is_none() && schema != "public" {
            found = reader.get_foreign_table("public", &table_ref.name)?;
        }
        found
    } else {
        reader.get_foreign_table(schema, &table_ref.name)?
    };

    if let Some(ft) = ftable {
        let columns = fdw_columns_to_column_defs(&ft);
        let n = columns.len();
        let bound = BoundTable {
            alias: table_ref.alias.clone(),
            name: table_ref.name.clone(),
            columns,
            col_offset: *col_offset,
        };
        *col_offset += n;
        return Ok(bound);
    }

    Err(DbError::TableNotFound {
        name: table_ref.name.clone(),
    })
}

/// Resolves a table for DML statements, trying the default schema first and
/// falling back to "public" for unqualified names when the search path has
/// a non-public first entry.
fn resolve_dml_table(
    reader: &mut CatalogReader,
    table_ref: &TableRef,
    default_database: &str,
    default_schema: &str,
) -> Result<axiomdb_catalog::TableDef, DbError> {
    let database = table_ref.database.as_deref().unwrap_or(default_database);
    let schema = table_ref.schema.as_deref().unwrap_or(default_schema);

    if table_ref.schema.is_none() {
        // Unqualified: try search_path order (default_schema, then public fallback)
        if let Some(td) = reader.get_table_in_database(database, schema, &table_ref.name)? {
            return Ok(td);
        }
        if schema != "public" {
            if let Some(td) = reader.get_table_in_database(database, "public", &table_ref.name)? {
                return Ok(td);
            }
        }
        Err(DbError::TableNotFound {
            name: table_ref.name.clone(),
        })
    } else {
        reader
            .get_table_in_database(database, schema, &table_ref.name)?
            .ok_or_else(|| DbError::TableNotFound {
                name: table_ref.name.clone(),
            })
    }
}

/// Convert ForeignTableDef columns to ColumnDef for binder binding (Phase 22b.2).
/// Uses the foreign table's own table_id (>= FOREIGN_TABLE_ID_BASE) so the
/// executor can detect and route to fdw_scan_table.
fn fdw_columns_to_column_defs(ft: &ForeignTableDef) -> Vec<ColumnDef> {
    use axiomdb_catalog::schema::TableId;
    ft.columns
        .iter()
        .enumerate()
        .map(|(i, fc)| ColumnDef {
            table_id: ft.table_id as TableId,
            col_idx: i as u16,
            name: fc.name.clone(),
            col_type: fc.col_type,
            nullable: fc.nullable,
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
        .collect()
}

/// Build virtual ColumnDef list from the SELECT list of an analyzed subquery.
fn virtual_columns_from_select(select: &SelectStmt) -> Vec<ColumnDef> {
    let mut out: Vec<ColumnDef> = Vec::with_capacity(select.columns.len());
    for (i, item) in select.columns.iter().enumerate() {
        match item {
            // Wildcards: try to expand using SRF column shapes when FROM is known.
            SelectItem::Wildcard => {
                let srf_cols = srf_wildcard_columns(&select.from);
                if let Some(cols) = srf_cols {
                    out.extend(cols);
                } else {
                    out.push(make_virtual_col(i as u16, format!("*{i}")));
                }
            }
            SelectItem::QualifiedWildcard(t) => {
                out.push(make_virtual_col(i as u16, format!("{t}.*{i}")));
            }
            SelectItem::Expr { alias: Some(a), .. } => {
                out.push(make_virtual_col(i as u16, a.clone()));
            }
            SelectItem::Expr {
                expr: Expr::Column { name, .. },
                ..
            } => {
                out.push(make_virtual_col(i as u16, split_name(name).1.to_string()));
            }
            SelectItem::Expr { .. } => {
                out.push(make_virtual_col(i as u16, format!("col{i}")));
            }
        }
    }
    out
}

fn make_virtual_col(col_idx: u16, name: String) -> axiomdb_catalog::schema::ColumnDef {
    use axiomdb_catalog::schema::ColumnType;
    axiomdb_catalog::schema::ColumnDef {
        table_id: 0,
        col_idx,
        name,
        col_type: ColumnType::Text,
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
    }
}

/// Expand a wildcard `SELECT *` to concrete column defs for known SRF FROM clauses.
/// Returns `None` for regular table references (let caller fall back to `*N` names).
fn srf_wildcard_columns(
    from: &Option<crate::ast::FromClause>,
) -> Option<Vec<axiomdb_catalog::schema::ColumnDef>> {
    match from {
        Some(crate::ast::FromClause::GenerateSeries(gs)) => {
            Some(crate::generate_series::column_defs_for_generate_series(gs))
        }
        Some(crate::ast::FromClause::Unnest(un)) => {
            Some(crate::unnest::column_defs_for_unnest(un))
        }
        // Phase 20.6 — READ_PARQUET: re-read schema from metadata (file exists
        // because bind already validated it; metadata read is fast).
        Some(crate::ast::FromClause::ReadParquet(rp)) => {
            parquet_schema_to_column_defs(&rp.path, &rp.column_aliases).ok()
        }
        _ => None,
    }
}
