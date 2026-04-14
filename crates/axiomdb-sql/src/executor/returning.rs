/// Phase 21.4 — shared RETURNING projection. Used by INSERT, UPDATE, and
/// DELETE executors to project the post-write (or pre-delete) row through
/// the user-supplied RETURNING list.
pub(crate) fn project_returning(
    items: &[SelectItem],
    rows: &[Vec<Value>],
    def: &TableDef,
    columns: &[CatalogColumnDef],
) -> Result<QueryResult, DbError> {
    let out_cols = expand_returning_cols(items, def, columns);
    let mut out_rows: Vec<Vec<Value>> = Vec::with_capacity(rows.len());
    for vals in rows {
        out_rows.push(project_returning_row(items, vals, columns)?);
    }
    Ok(QueryResult::Rows {
        columns: out_cols,
        rows: out_rows,
    })
}

pub(crate) fn expand_returning_cols(
    items: &[SelectItem],
    def: &TableDef,
    columns: &[CatalogColumnDef],
) -> Vec<ColumnMeta> {
    let mut out = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                for c in columns {
                    out.push(ColumnMeta {
                        name: c.name.clone(),
                        data_type: crate::table::column_type_to_data_type(c.col_type),
                        nullable: c.nullable,
                        table_name: Some(def.table_name.clone()),
                    });
                }
            }
            SelectItem::Expr { expr, alias } => {
                let name = alias
                    .clone()
                    .unwrap_or_else(|| expr_column_name(expr, None));
                let data_type = if let Expr::Column { col_idx, .. } = expr {
                    columns
                        .get(*col_idx)
                        .map(|c| crate::table::column_type_to_data_type(c.col_type))
                        .unwrap_or(DataType::Text)
                } else {
                    DataType::Text
                };
                out.push(ColumnMeta {
                    name,
                    data_type,
                    nullable: true,
                    table_name: Some(def.table_name.clone()),
                });
            }
        }
    }
    out
}

pub(crate) fn project_returning_row(
    items: &[SelectItem],
    row: &[Value],
    columns: &[CatalogColumnDef],
) -> Result<Vec<Value>, DbError> {
    let mut out = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                for i in 0..columns.len() {
                    out.push(row.get(i).cloned().unwrap_or(Value::Null));
                }
            }
            SelectItem::Expr { expr, .. } => {
                out.push(eval(expr, row)?);
            }
        }
    }
    Ok(out)
}
