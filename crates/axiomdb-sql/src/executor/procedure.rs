// ── Stored procedure CALL execution (Phase 16.7) ───────────────────────────────
//
// Step 9 resolves the target procedure and turns an unknown CALL into a real
// `ProcedureNotFound` error (replacing the old silent no-op). The tree-walking
// interpreter that runs a found procedure's body lands in Step 10.

/// Resolves a procedure by (optionally schema-qualified) name. Unqualified names
/// are searched along the session `search_path`. Returns `None` if not found.
fn resolve_procedure(
    reader: &mut CatalogReader,
    name: &str,
    search_path: &[String],
) -> Result<Option<axiomdb_catalog::ProcedureDef>, DbError> {
    if let Some((schema, proc)) = name.split_once('.') {
        return reader.get_procedure(schema, proc);
    }
    for schema in search_path {
        if let Some(def) = reader.get_procedure(schema, name)? {
            return Ok(Some(def));
        }
    }
    Ok(None)
}

/// Executes `CALL name(args)`.
///
/// Replaces the former silent no-op: an unknown procedure now returns
/// `ProcedureNotFound`. (Body execution is wired in Step 10.)
fn execute_call_ctx(
    name: &str,
    _args: &[crate::expr::Expr],
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let conn = ctx
        .conn_txn
        .as_ref()
        .expect("conn_txn must be set before dispatch_ctx");
    let snap = txn.active_snapshot(conn);
    let mut reader = CatalogReader::new(storage, snap)?;
    let def = resolve_procedure(&mut reader, name, &ctx.search_path)?;
    let Some(_def) = def else {
        return Err(DbError::ProcedureNotFound {
            name: name.to_string(),
        });
    };

    // Step 10: run the procedure body via the tree-walking interpreter.
    Err(DbError::NotImplemented {
        feature: "stored procedure body execution (lands in the next step)".into(),
    })
}
