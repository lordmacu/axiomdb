// ── Stored procedure CALL execution (Phase 16.7) ───────────────────────────────
//
// Tree-walking interpreter for stored procedures. A CALL resolves the procedure,
// binds IN/INOUT arguments and DECLARE locals into a variable frame, re-parses the
// body, and runs each statement in the caller's transaction. Frame variables are
// substituted (as literals) into each statement's expressions before execution, so
// the embedded SQL never tries to resolve a variable name as a column. OUT/INOUT
// parameters are returned to the caller as a one-row result set.

use axiomdb_catalog::{ProcParamMode, ProcedureDef};
use axiomdb_types::{coerce, CoercionMode};

/// A single procedure-local variable (a parameter or a DECLARE'd local).
struct ProcVar {
    name: String,
    /// `Some` for a formal parameter; `None` for a DECLARE'd local.
    mode: Option<ProcParamMode>,
    ty: DataType,
    value: Value,
}

/// The variable frame for one procedure activation: parameters (in declaration
/// order) followed by DECLARE'd locals.
struct ProcFrame {
    vars: Vec<ProcVar>,
}

impl ProcFrame {
    /// Case-insensitive lookup of a variable's current value.
    fn get(&self, name: &str) -> Option<&Value> {
        self.vars
            .iter()
            .find(|v| v.name.eq_ignore_ascii_case(name))
            .map(|v| &v.value)
    }

    /// Assigns to a variable, coercing to its declared type. Errors if the name
    /// is undeclared or refers to an `IN` parameter (which is read-only).
    fn assign(&mut self, name: &str, value: Value) -> Result<(), DbError> {
        let var = self
            .vars
            .iter_mut()
            .find(|v| v.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| DbError::InvalidValue {
                reason: format!("\"{name}\" is not a declared variable in this procedure"),
            })?;
        if var.mode == Some(ProcParamMode::In) {
            return Err(DbError::InvalidValue {
                reason: format!("cannot assign to IN parameter \"{name}\""),
            });
        }
        var.value = coerce(value, var.ty.clone(), CoercionMode::Permissive)?;
        Ok(())
    }
}

/// Resolves a procedure by (optionally schema-qualified) name. Unqualified names
/// are searched along the session `search_path`. Returns `None` if not found.
fn resolve_procedure(
    reader: &mut CatalogReader,
    name: &str,
    search_path: &[String],
) -> Result<Option<ProcedureDef>, DbError> {
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

/// Substitutes frame variables (matched case-insensitively by name) with their
/// current values, as `Expr::Literal`, throughout a scalar expression. Mirrors
/// the variant coverage of `substitute_outer_columns`; advanced variants and
/// subqueries are cloned as-is (v1 does not substitute frame vars inside a
/// subquery body — use a flat expression or pass the value via a DECLARE).
fn subst_vars_expr(expr: &Expr, frame: &ProcFrame) -> Expr {
    match expr {
        // Leaf: an unqualified identifier matching a frame variable.
        Expr::Column { name, .. } => match frame.get(name) {
            Some(v) => Expr::Literal(v.clone()),
            None => expr.clone(),
        },
        // No nested expressions / not recursed into (v1).
        Expr::Literal(_)
        | Expr::Default
        | Expr::Param { .. }
        | Expr::InsertValue { .. }
        | Expr::ExcludedValue { .. }
        | Expr::OuterColumn { .. }
        | Expr::Subquery(_)
        | Expr::InSubquery { .. }
        | Expr::Exists { .. } => expr.clone(),
        Expr::UnaryOp { op, operand } => Expr::UnaryOp {
            op: *op,
            operand: Box::new(subst_vars_expr(operand, frame)),
        },
        Expr::BinaryOp { op, left, right } => Expr::BinaryOp {
            op: *op,
            left: Box::new(subst_vars_expr(left, frame)),
            right: Box::new(subst_vars_expr(right, frame)),
        },
        Expr::Collate { expr, collation } => Expr::Collate {
            expr: Box::new(subst_vars_expr(expr, frame)),
            collation: collation.clone(),
        },
        Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: Box::new(subst_vars_expr(expr, frame)),
            negated: *negated,
        },
        Expr::IsBoolean {
            expr,
            value,
            negated,
        } => Expr::IsBoolean {
            expr: Box::new(subst_vars_expr(expr, frame)),
            value: *value,
            negated: *negated,
        },
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => Expr::Between {
            expr: Box::new(subst_vars_expr(expr, frame)),
            low: Box::new(subst_vars_expr(low, frame)),
            high: Box::new(subst_vars_expr(high, frame)),
            negated: *negated,
        },
        Expr::Like {
            expr,
            pattern,
            negated,
            escape,
        } => Expr::Like {
            expr: Box::new(subst_vars_expr(expr, frame)),
            pattern: Box::new(subst_vars_expr(pattern, frame)),
            negated: *negated,
            escape: escape.as_ref().map(|e| Box::new(subst_vars_expr(e, frame))),
        },
        Expr::In {
            expr,
            list,
            negated,
        } => Expr::In {
            expr: Box::new(subst_vars_expr(expr, frame)),
            list: list.iter().map(|e| subst_vars_expr(e, frame)).collect(),
            negated: *negated,
        },
        Expr::Function { name, args } => Expr::Function {
            name: name.clone(),
            args: args.iter().map(|e| subst_vars_expr(e, frame)).collect(),
        },
        Expr::Case {
            operand,
            when_thens,
            else_result,
        } => Expr::Case {
            operand: operand
                .as_ref()
                .map(|e| Box::new(subst_vars_expr(e, frame))),
            when_thens: when_thens
                .iter()
                .map(|(w, t)| (subst_vars_expr(w, frame), subst_vars_expr(t, frame)))
                .collect(),
            else_result: else_result
                .as_ref()
                .map(|e| Box::new(subst_vars_expr(e, frame))),
        },
        Expr::Cast { expr, target } => Expr::Cast {
            expr: Box::new(subst_vars_expr(expr, frame)),
            target: target.clone(),
        },
        Expr::ArrayConstructor { elements } => Expr::ArrayConstructor {
            elements: elements.iter().map(|e| subst_vars_expr(e, frame)).collect(),
        },
        // Window functions, JSON-query, aggregates, etc. — frame vars do not
        // appear there in v1 bodies; clone unchanged.
        other => other.clone(),
    }
}

/// Applies frame-variable substitution to the value/predicate expressions of a
/// body statement (INSERT VALUES, UPDATE SET/WHERE, DELETE WHERE, CALL args).
fn subst_vars_stmt(stmt: Stmt, frame: &ProcFrame) -> Stmt {
    match stmt {
        Stmt::Insert(mut s) => {
            if let crate::ast::InsertSource::Values(rows) = &mut s.source {
                for row in rows.iter_mut() {
                    for e in row.iter_mut() {
                        *e = subst_vars_expr(e, frame);
                    }
                }
            }
            Stmt::Insert(s)
        }
        Stmt::Update(mut s) => {
            for a in s.assignments.iter_mut() {
                a.value = subst_vars_expr(&a.value, frame);
            }
            if let Some(w) = &s.where_clause {
                s.where_clause = Some(subst_vars_expr(w, frame));
            }
            Stmt::Update(s)
        }
        Stmt::Delete(mut s) => {
            if let Some(w) = &s.where_clause {
                s.where_clause = Some(subst_vars_expr(w, frame));
            }
            Stmt::Delete(s)
        }
        Stmt::Call { name, args } => Stmt::Call {
            name,
            args: args.iter().map(|e| subst_vars_expr(e, frame)).collect(),
        },
        other => other,
    }
}

/// Evaluates a scalar expression (with frame variables already substituted) in
/// the current session, supporting scalar subqueries.
fn eval_proc_expr(
    expr: &Expr,
    exec_ctx: &ExecutionContext,
    ctx: &mut SessionContext,
) -> Result<Value, DbError> {
    let mut runner = ExecSubqueryRunner {
        storage: exec_ctx.storage(),
        txn: exec_ctx.coord(),
        bloom: exec_ctx.bloom(),
        ctx,
        outer_row: &[],
        cache: None,
        in_set_cache: None,
        correlated_cache: None,
        materialized: None,
    };
    crate::eval::eval_with(expr, &[], &mut runner)
}

/// Executes `CALL name(args)` with a recursion-depth guard around the body
/// interpreter, so mutual/self recursion cannot overflow the stack.
fn execute_call_ctx(
    name: &str,
    args: &[Expr],
    exec_ctx: &ExecutionContext,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    ctx.proc_call_depth += 1;
    if ctx.proc_call_depth > crate::session::MAX_PROC_CALL_DEPTH {
        ctx.proc_call_depth -= 1;
        return Err(DbError::InvalidValue {
            reason: format!(
                "stored procedure recursion depth limit ({}) exceeded",
                crate::session::MAX_PROC_CALL_DEPTH
            ),
        });
    }
    let result = execute_call_inner(name, args, exec_ctx, ctx);
    ctx.proc_call_depth -= 1;
    result
}

/// Runs the procedure body in the caller's transaction. Returns a one-row result
/// set of OUT/INOUT parameter values, or `Empty` when there are none.
fn execute_call_inner(
    name: &str,
    args: &[Expr],
    exec_ctx: &ExecutionContext,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    // Resolve the procedure under the statement snapshot.
    let def = {
        let conn = ctx
            .conn_txn
            .as_ref()
            .expect("conn_txn must be set before dispatch_ctx");
        let snap = exec_ctx.coord().active_snapshot(conn);
        let mut reader = CatalogReader::new(exec_ctx.storage(), snap)?;
        match resolve_procedure(&mut reader, name, &ctx.search_path)? {
            Some(d) => d,
            None => {
                return Err(DbError::ProcedureNotFound {
                    name: name.to_string(),
                })
            }
        }
    };

    // Arity: positional, one argument per parameter (PostgreSQL CALL semantics —
    // OUT parameters take a placeholder argument that is ignored).
    if args.len() != def.params.len() {
        return Err(DbError::InvalidValue {
            reason: format!(
                "procedure \"{}\" expects {} argument(s), got {}",
                def.name,
                def.params.len(),
                args.len()
            ),
        });
    }

    // ── Build the variable frame ──────────────────────────────────────────────
    let mut frame = ProcFrame {
        vars: Vec::with_capacity(def.params.len()),
    };
    for (param, arg) in def.params.iter().zip(args.iter()) {
        let ty = crate::table::column_type_to_data_type(param.data_type);
        let value = match param.mode {
            // IN / INOUT bind from the (caller-context) argument.
            ProcParamMode::In | ProcParamMode::InOut => {
                let v = eval_proc_expr(arg, exec_ctx, ctx)?;
                coerce(v, ty.clone(), CoercionMode::Permissive)?
            }
            // OUT starts NULL; its argument is a placeholder and ignored.
            ProcParamMode::Out => Value::Null,
        };
        frame.vars.push(ProcVar {
            name: param.name.clone(),
            mode: Some(param.mode),
            ty,
            value,
        });
    }

    // Re-parse the body and initialize DECLARE'd locals.
    let sql_mode = ctx.sql_mode_flags();
    let body = crate::parser::proc_body::parse_proc_body(&def.body_sql, def.language, sql_mode)?;
    for decl in &body.declares {
        let value = match &decl.init {
            Some(init_expr) => {
                let substituted = subst_vars_expr(init_expr, &frame);
                let v = eval_proc_expr(&substituted, exec_ctx, ctx)?;
                coerce(v, decl.ty.clone(), CoercionMode::Permissive)?
            }
            None => Value::Null,
        };
        frame.vars.push(ProcVar {
            name: decl.name.clone(),
            mode: None,
            ty: decl.ty.clone(),
            value,
        });
    }

    // ── Execute the body sequentially ─────────────────────────────────────────
    for ps in &body.statements {
        match ps {
            crate::ast::ProcStmt::Assign { target, value } => {
                let substituted = subst_vars_expr(value, &frame);
                let v = eval_proc_expr(&substituted, exec_ctx, ctx)?;
                frame.assign(target, v)?;
            }
            crate::ast::ProcStmt::Sql(stmt) => {
                let substituted = subst_vars_stmt((**stmt).clone(), &frame);
                dispatch_ctx(substituted, exec_ctx, ctx)?;
            }
            // SELECT … INTO is parsed away in v1 (deferred); never produced.
            crate::ast::ProcStmt::SelectInto { .. } => {
                return Err(DbError::NotImplemented {
                    feature: "SELECT … INTO in procedure bodies (deferred)".into(),
                });
            }
        }
    }

    // ── Surface OUT / INOUT parameters as a one-row result set ─────────────────
    let out_vars: Vec<&ProcVar> = frame
        .vars
        .iter()
        .filter(|v| matches!(v.mode, Some(ProcParamMode::Out | ProcParamMode::InOut)))
        .collect();
    if out_vars.is_empty() {
        return Ok(QueryResult::Empty);
    }
    let columns = out_vars
        .iter()
        .map(|v| ColumnMeta::computed(v.name.clone(), v.ty.clone()))
        .collect();
    let row = out_vars.iter().map(|v| v.value.clone()).collect();
    Ok(QueryResult::Rows {
        columns,
        rows: vec![row],
    })
}
