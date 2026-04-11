// ── GROUP BY key hashing ──────────────────────────────────────────────────────

/// Serializes a `Value` to a self-describing byte sequence for use as a
/// GROUP BY hash key.
///
/// Properties:
/// - Two `NULL` values produce identical bytes `[0x00]` → they form one group
///   (SQL grouping semantics: NULLs are considered equal for GROUP BY).
/// - `Real(f64)` uses `to_bits()` for bit-exact representation. `NaN` would
///   produce a fixed bit pattern, but NaN is forbidden in stored values.
/// - The tag byte guarantees values of different types never collide.
fn value_to_key_bytes(v: &Value) -> Vec<u8> {
    let mut buf = Vec::new();
    match v {
        Value::Null => buf.push(0x00),
        Value::Bool(b) => {
            buf.push(0x01);
            buf.push(*b as u8);
        }
        Value::Int(n) => {
            buf.push(0x02);
            buf.extend_from_slice(&n.to_le_bytes());
        }
        Value::BigInt(n) => {
            buf.push(0x03);
            buf.extend_from_slice(&n.to_le_bytes());
        }
        Value::Real(f) => {
            buf.push(0x04);
            buf.extend_from_slice(&f.to_bits().to_le_bytes());
        }
        Value::Decimal(m, s) => {
            buf.push(0x05);
            buf.extend_from_slice(&m.to_le_bytes());
            buf.push(*s);
        }
        Value::Text(s) | Value::Json(s) => {
            buf.push(0x06);
            buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
            buf.extend_from_slice(s.as_bytes());
        }
        Value::Jsonb(b) => {
            buf.push(0x0B);
            buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
            buf.extend_from_slice(b.as_ref());
        }
        Value::Bytes(b) => {
            buf.push(0x07);
            buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
            buf.extend_from_slice(b);
        }
        Value::Date(d) => {
            buf.push(0x08);
            buf.extend_from_slice(&d.to_le_bytes());
        }
        Value::Timestamp(t) => {
            buf.push(0x09);
            buf.extend_from_slice(&t.to_le_bytes());
        }
        Value::Uuid(u) => {
            buf.push(0x0A);
            buf.extend_from_slice(u.as_slice());
        }
    }
    buf
}

// ── HAVING evaluator ──────────────────────────────────────────────────────────

/// Evaluates a HAVING expression against a finalized group.
///
/// `Expr::Column` references are evaluated against `representative_row`
/// (the original source row, so `col_idx` values from the analyzer are valid).
///
/// `Expr::Function` aggregate calls are looked up in `agg_values` by name + arg.
///
/// All other expressions are evaluated by delegating sub-expression results to
/// the standard `eval()` via synthetic `Expr::Literal` nodes.
fn eval_with_aggs(
    expr: &Expr,
    representative_row: &[Value],
    agg_values: &[Value],
    agg_exprs: &[AggExpr],
) -> Result<Value, DbError> {
    match expr {
        Expr::Literal(v) => Ok(v.clone()),

        Expr::Column { col_idx, .. } => {
            representative_row
                .get(*col_idx)
                .cloned()
                .ok_or(DbError::ColumnIndexOutOfBounds {
                    idx: *col_idx,
                    len: representative_row.len(),
                })
        }

        // GROUP_CONCAT: look up the pre-computed finalized value by structural match.
        Expr::GroupConcat {
            expr: gc_expr,
            distinct,
            order_by,
            separator,
        } => {
            let idx = agg_exprs
                .iter()
                .position(|ae| ae.matches_group_concat(gc_expr, *distinct, order_by, separator))
                .ok_or_else(|| {
                    DbError::Other("GROUP_CONCAT not pre-registered — internal error".to_string())
                })?;
            Ok(agg_values[idx].clone())
        }

        Expr::Function { name, args } if is_aggregate(name.as_str()) => {
            let idx = agg_exprs
                .iter()
                .position(|ae| ae.matches_simple(name.as_str(), args))
                .ok_or_else(|| {
                    DbError::Other(format!(
                        "aggregate '{name}' not pre-registered — internal error"
                    ))
                })?;
            Ok(agg_values[idx].clone())
        }

        // AND: short-circuit
        Expr::BinaryOp {
            op: BinaryOp::And,
            left,
            right,
        } => {
            let l = eval_with_aggs(left, representative_row, agg_values, agg_exprs)?;
            match l {
                Value::Bool(false) => Ok(Value::Bool(false)),
                Value::Bool(true) => {
                    eval_with_aggs(right, representative_row, agg_values, agg_exprs)
                }
                Value::Null => {
                    let r = eval_with_aggs(right, representative_row, agg_values, agg_exprs)?;
                    Ok(if matches!(r, Value::Bool(false)) {
                        Value::Bool(false)
                    } else {
                        Value::Null
                    })
                }
                other => Err(DbError::TypeMismatch {
                    expected: "Bool".into(),
                    got: other.variant_name().into(),
                }),
            }
        }

        // OR: short-circuit
        Expr::BinaryOp {
            op: BinaryOp::Or,
            left,
            right,
        } => {
            let l = eval_with_aggs(left, representative_row, agg_values, agg_exprs)?;
            match l {
                Value::Bool(true) => Ok(Value::Bool(true)),
                Value::Bool(false) => {
                    eval_with_aggs(right, representative_row, agg_values, agg_exprs)
                }
                Value::Null => {
                    let r = eval_with_aggs(right, representative_row, agg_values, agg_exprs)?;
                    Ok(if matches!(r, Value::Bool(true)) {
                        Value::Bool(true)
                    } else {
                        Value::Null
                    })
                }
                other => Err(DbError::TypeMismatch {
                    expected: "Bool".into(),
                    got: other.variant_name().into(),
                }),
            }
        }

        // Other binary ops: evaluate both sides, delegate to eval() via Literal.
        Expr::BinaryOp { op, left, right } => {
            let l = eval_with_aggs(left, representative_row, agg_values, agg_exprs)?;
            let r = eval_with_aggs(right, representative_row, agg_values, agg_exprs)?;
            eval(
                &Expr::BinaryOp {
                    op: *op,
                    left: Box::new(Expr::Literal(l)),
                    right: Box::new(Expr::Literal(r)),
                },
                &[],
            )
        }

        Expr::UnaryOp { op, operand } => {
            let v = eval_with_aggs(operand, representative_row, agg_values, agg_exprs)?;
            eval(
                &Expr::UnaryOp {
                    op: *op,
                    operand: Box::new(Expr::Literal(v)),
                },
                &[],
            )
        }

        Expr::IsNull { expr, negated } => {
            let v = eval_with_aggs(expr, representative_row, agg_values, agg_exprs)?;
            eval(
                &Expr::IsNull {
                    expr: Box::new(Expr::Literal(v)),
                    negated: *negated,
                },
                &[],
            )
        }

        Expr::IsBoolean {
            expr,
            value,
            negated,
        } => {
            let v = eval_with_aggs(expr, representative_row, agg_values, agg_exprs)?;
            eval(
                &Expr::IsBoolean {
                    expr: Box::new(Expr::Literal(v)),
                    value: *value,
                    negated: *negated,
                },
                &[],
            )
        }

        // CASE WHEN in HAVING context — recurse through eval_with_aggs for sub-exprs.
        Expr::Case {
            operand,
            when_thens,
            else_result,
        } => {
            match operand {
                None => {
                    for (when_expr, then_expr) in when_thens {
                        let cond =
                            eval_with_aggs(when_expr, representative_row, agg_values, agg_exprs)?;
                        if is_truthy(&cond) {
                            return eval_with_aggs(
                                then_expr,
                                representative_row,
                                agg_values,
                                agg_exprs,
                            );
                        }
                    }
                }
                Some(base_expr) => {
                    let base_val =
                        eval_with_aggs(base_expr, representative_row, agg_values, agg_exprs)?;
                    for (val_expr, then_expr) in when_thens {
                        let val =
                            eval_with_aggs(val_expr, representative_row, agg_values, agg_exprs)?;
                        let eq = eval(
                            &Expr::BinaryOp {
                                op: BinaryOp::Eq,
                                left: Box::new(Expr::Literal(base_val.clone())),
                                right: Box::new(Expr::Literal(val)),
                            },
                            &[],
                        )?;
                        if is_truthy(&eq) {
                            return eval_with_aggs(
                                then_expr,
                                representative_row,
                                agg_values,
                                agg_exprs,
                            );
                        }
                    }
                }
            }
            match else_result {
                Some(else_expr) => {
                    eval_with_aggs(else_expr, representative_row, agg_values, agg_exprs)
                }
                None => Ok(Value::Null),
            }
        }

        // Compound predicates that may contain aggregates in sub-expressions.
        Expr::Like {
            expr,
            pattern,
            negated,
            escape,
        } => {
            let v = eval_with_aggs(expr, representative_row, agg_values, agg_exprs)?;
            let p = eval_with_aggs(pattern, representative_row, agg_values, agg_exprs)?;
            eval(
                &Expr::Like {
                    expr: Box::new(Expr::Literal(v)),
                    pattern: Box::new(Expr::Literal(p)),
                    negated: *negated,
                    escape: escape.clone(),
                },
                &[],
            )
        }

        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => {
            let v = eval_with_aggs(expr, representative_row, agg_values, agg_exprs)?;
            let lo = eval_with_aggs(low, representative_row, agg_values, agg_exprs)?;
            let hi = eval_with_aggs(high, representative_row, agg_values, agg_exprs)?;
            eval(
                &Expr::Between {
                    expr: Box::new(Expr::Literal(v)),
                    low: Box::new(Expr::Literal(lo)),
                    high: Box::new(Expr::Literal(hi)),
                    negated: *negated,
                },
                &[],
            )
        }

        Expr::In {
            expr,
            list,
            negated,
        } => {
            let v = eval_with_aggs(expr, representative_row, agg_values, agg_exprs)?;
            let evaluated_list: Result<Vec<Expr>, _> = list
                .iter()
                .map(|e| {
                    eval_with_aggs(e, representative_row, agg_values, agg_exprs).map(Expr::Literal)
                })
                .collect();
            eval(
                &Expr::In {
                    expr: Box::new(Expr::Literal(v)),
                    list: evaluated_list?,
                    negated: *negated,
                },
                &[],
            )
        }

        Expr::Cast { expr, target } => {
            let v = eval_with_aggs(expr, representative_row, agg_values, agg_exprs)?;
            eval(
                &Expr::Cast {
                    expr: Box::new(Expr::Literal(v)),
                    target: *target,
                },
                &[],
            )
        }

        // For remaining variants: fall back to standard eval against representative_row.
        other => eval(other, representative_row),
    }
}

/// Resolves SELECT aliases in a HAVING expression (G8 / 4.21b).
///
/// MySQL allows HAVING to reference column aliases defined in the SELECT list:
///   `SELECT dept, COUNT(*) AS cnt FROM t GROUP BY dept HAVING cnt > 5`
///
/// This function walks the HAVING expression and replaces any `Expr::Column`
/// whose name matches a SELECT alias with the corresponding SELECT expression.
/// Called before `eval_with_aggs` so the replaced expressions are evaluated
/// via the normal aggregate path.
fn resolve_having_aliases(expr: Expr, select_items: &[SelectItem]) -> Expr {
    // Build alias → expression map from SELECT list.
    let alias_map: std::collections::HashMap<String, Expr> = select_items
        .iter()
        .filter_map(|item| {
            if let SelectItem::Expr {
                expr,
                alias: Some(alias),
            } = item
            {
                Some((alias.to_ascii_lowercase(), expr.clone()))
            } else {
                None
            }
        })
        .collect();

    if alias_map.is_empty() {
        return expr;
    }
    subst_having_aliases(expr, &alias_map)
}

fn subst_having_aliases(
    expr: Expr,
    alias_map: &std::collections::HashMap<String, Expr>,
) -> Expr {
    match expr {
        // Replace a bare column reference that matches an alias.
        Expr::Column { ref name, .. } => {
            if let Some(replacement) = alias_map.get(&name.to_ascii_lowercase()) {
                replacement.clone()
            } else {
                expr
            }
        }
        // Recurse into compound expressions.
        Expr::BinaryOp { op, left, right } => Expr::BinaryOp {
            op,
            left: Box::new(subst_having_aliases(*left, alias_map)),
            right: Box::new(subst_having_aliases(*right, alias_map)),
        },
        Expr::UnaryOp { op, operand } => Expr::UnaryOp {
            op,
            operand: Box::new(subst_having_aliases(*operand, alias_map)),
        },
        Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: Box::new(subst_having_aliases(*expr, alias_map)),
            negated,
        },
        Expr::IsBoolean {
            expr,
            value,
            negated,
        } => Expr::IsBoolean {
            expr: Box::new(subst_having_aliases(*expr, alias_map)),
            value,
            negated,
        },
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => Expr::Between {
            expr: Box::new(subst_having_aliases(*expr, alias_map)),
            low: Box::new(subst_having_aliases(*low, alias_map)),
            high: Box::new(subst_having_aliases(*high, alias_map)),
            negated,
        },
        Expr::In { expr, list, negated } => Expr::In {
            expr: Box::new(subst_having_aliases(*expr, alias_map)),
            list: list
                .into_iter()
                .map(|e| subst_having_aliases(e, alias_map))
                .collect(),
            negated,
        },
        Expr::Function { name, args } => Expr::Function {
            name,
            args: args
                .into_iter()
                .map(|a| subst_having_aliases(a, alias_map))
                .collect(),
        },
        Expr::Cast { expr, target } => Expr::Cast {
            expr: Box::new(subst_having_aliases(*expr, alias_map)),
            target,
        },
        other => other,
    }
}

