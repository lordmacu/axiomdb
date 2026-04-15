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
                if col.name == col_name {
                    found.push((ti, table.col_offset + ci, col.name.as_str()));
                }
            }
        }
        found
    }

    /// Resolve a qualified `table.column` or unqualified `column` reference.
    fn resolve_column(&self, name: &str) -> Result<usize, DbError> {
        let (qualifier, field) = split_name(name);

        if let Some(q) = qualifier {
            // Qualified: find the specific table
            let table = self.find_table(q).ok_or_else(|| DbError::TableNotFound {
                name: format!("{q}.{field}"),
            })?;
            let (pos, _) = find_col_in_table(table, field, q)?;
            Ok(table.col_offset + pos)
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
                1 => Ok(matches[0].1),
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
        .find(|(_, c)| c.name == col_name)
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
        )?;
        ctx.tables.extend(bound);
    }

    for join in joins {
        let bound = bound_from_clause(
            &join.table,
            storage,
            snapshot.clone(),
            default_database,
            default_schema,
            &mut col_offset,
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
        FromClause::Subquery { query, alias, .. } => {
            // Analyze the inner SELECT recursively.
            // The virtual table's columns = the SELECT list items.
            let analyzed_query = analyze_select(
                *query.clone(),
                storage,
                snapshot,
                default_database,
                default_schema,
            )?;
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
    }
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
    }
    .ok_or_else(|| DbError::TableNotFound {
        name: table_ref.name.clone(),
    })?;

    let columns = reader.list_columns(table_def.id)?;
    let n = columns.len();
    let bound = BoundTable {
        alias: table_ref.alias.clone(),
        name: table_ref.name.clone(),
        columns,
        col_offset: *col_offset,
    };
    *col_offset += n;
    Ok(bound)
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

/// Build virtual ColumnDef list from the SELECT list of an analyzed subquery.
fn virtual_columns_from_select(select: &SelectStmt) -> Vec<ColumnDef> {
    use axiomdb_catalog::schema::{ColumnType, TableId};
    select
        .columns
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let name = match item {
                SelectItem::Expr { alias: Some(a), .. } => a.clone(),
                SelectItem::Expr {
                    expr: Expr::Column { name, .. },
                    ..
                } => {
                    // Use the unqualified column name if no alias
                    split_name(name).1.to_string()
                }
                SelectItem::Expr { .. } => format!("col{i}"),
                SelectItem::Wildcard => format!("*{i}"),
                SelectItem::QualifiedWildcard(t) => format!("{t}.*{i}"),
            };
            ColumnDef {
                table_id: 0 as TableId,
                col_idx: i as u16,
                name,
                col_type: ColumnType::Text, // type unknown without full type inference
                nullable: true,
                auto_increment: false,
                type_len: 0,
                is_fixed_len: false,
                default_expr: None,
                on_update_expr: None,
            }
        })
        .collect()
}
