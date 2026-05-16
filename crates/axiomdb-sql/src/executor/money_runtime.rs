// Money scalar functions (Phase 20.17):
//   MONEY(amount, 'USD')           → MONEY value
//   convert_currency(money, 'USD') → MONEY value (via CONVERT(money, 'USD'))
//   CURRENCY_OF(money)             → TEXT
//   AMOUNT_OF(money)               → DECIMAL
//
// Included into the executor module (mod.rs) so it shares all imports.
// Exchange rates are loaded from the catalog once per (session, from, to) pair
// and cached in `ctx.exchange_rate_cache`.

fn eval_money_function(
    name: &str,
    args: &[Expr],
    row: &[Value],
    runner: &mut ExecSubqueryRunner<'_>,
) -> Result<Option<Value>, DbError> {
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "money" => money_constructor(args, row, runner).map(Some),
        "convert_currency" => convert_currency_fn(args, row, runner).map(Some),
        "currency_of" => currency_of_fn(args, row, runner).map(Some),
        "amount_of" => amount_of_fn(args, row, runner).map(Some),
        _ => Ok(None),
    }
}

// ── MONEY(amount, 'USD') ──────────────────────────────────────────────────────

fn money_constructor(
    args: &[Expr],
    row: &[Value],
    runner: &mut ExecSubqueryRunner<'_>,
) -> Result<Value, DbError> {
    if args.len() != 2 {
        return Err(DbError::TypeMismatch {
            expected: "MONEY(amount, currency_code)".into(),
            got: format!("{} argument(s)", args.len()),
        });
    }
    let amount = crate::eval::eval_with(&args[0], row, runner)?;
    let currency_raw = crate::eval::eval_with(&args[1], row, runner)?;

    let (mantissa, scale) = match amount {
        Value::Decimal(m, s) => (m, s),
        Value::Int(n) => (n as i128, 0u8),
        Value::BigInt(n) => (n as i128, 0u8),
        Value::Real(f) => {
            let s = 6u8;
            let m = (f * 1_000_000.0).round() as i128;
            (m, s)
        }
        Value::Text(ref s) => {
            use axiomdb_types::{coerce, CoercionMode, DataType};
            match coerce(Value::Text(s.clone()), DataType::Decimal, CoercionMode::Strict) {
                Ok(Value::Decimal(m, sc)) => (m, sc),
                _ => {
                    return Err(DbError::TypeMismatch {
                        expected: "numeric amount for MONEY()".into(),
                        got: s.clone(),
                    });
                }
            }
        }
        other => {
            return Err(DbError::TypeMismatch {
                expected: "numeric amount for MONEY()".into(),
                got: other.variant_name().to_string(),
            });
        }
    };

    let currency_code = match currency_raw {
        Value::Text(s) => s.to_ascii_uppercase(),
        other => {
            return Err(DbError::TypeMismatch {
                expected: "currency code string for MONEY()".into(),
                got: other.variant_name().to_string(),
            });
        }
    };

    if currency_code.len() != 3 || !currency_code.chars().all(|c| c.is_ascii_alphabetic()) {
        return Err(DbError::InvalidValue {
            reason: format!("currency code '{currency_code}' must be 3 ASCII letters (ISO 4217)"),
        });
    }

    let mut code_bytes = [0u8; 3];
    code_bytes.copy_from_slice(currency_code.as_bytes());
    Ok(Value::Money(mantissa, scale, code_bytes))
}

// ── convert_currency(money, 'USD') ────────────────────────────────────────────

fn convert_currency_fn(
    args: &[Expr],
    row: &[Value],
    runner: &mut ExecSubqueryRunner<'_>,
) -> Result<Value, DbError> {
    if args.len() != 2 {
        return Err(DbError::TypeMismatch {
            expected: "CONVERT(money_expr, 'target_currency')".into(),
            got: format!("{} argument(s)", args.len()),
        });
    }
    let money_val = crate::eval::eval_with(&args[0], row, runner)?;
    let target_raw = crate::eval::eval_with(&args[1], row, runner)?;

    let (src_m, src_s, src_code) = match money_val {
        Value::Money(m, s, c) => (m, s, c),
        Value::Null => return Ok(Value::Null),
        other => {
            return Err(DbError::TypeMismatch {
                expected: "MONEY value for CONVERT()".into(),
                got: other.variant_name().to_string(),
            });
        }
    };

    let target = match target_raw {
        Value::Text(s) => s.to_ascii_uppercase(),
        Value::Null => return Ok(Value::Null),
        other => {
            return Err(DbError::TypeMismatch {
                expected: "currency code string for CONVERT()".into(),
                got: other.variant_name().to_string(),
            });
        }
    };

    let src = std::str::from_utf8(&src_code)
        .map_err(|_| DbError::InvalidValue { reason: "invalid money currency code".into() })?
        .to_ascii_uppercase();

    if src == target {
        let mut code_bytes = [0u8; 3];
        code_bytes.copy_from_slice(target.as_bytes());
        return Ok(Value::Money(src_m, src_s, code_bytes));
    }

    let (rate_m, rate_s) = get_or_load_exchange_rate(&src, &target, runner)?;

    // result = src_mantissa × rate_mantissa at scale = src_scale + rate_scale
    let result_m = src_m
        .checked_mul(rate_m)
        .ok_or_else(|| DbError::InvalidValue { reason: "currency conversion overflow".into() })?;
    let result_s = src_s.saturating_add(rate_s);

    let mut code_bytes = [0u8; 3];
    code_bytes.copy_from_slice(target.as_bytes());
    Ok(Value::Money(result_m, result_s, code_bytes))
}

/// Loads (mantissa, scale) for from→to exchange rate from session cache or catalog.
fn get_or_load_exchange_rate(
    from: &str,
    to: &str,
    runner: &mut ExecSubqueryRunner<'_>,
) -> Result<(i128, u8), DbError> {
    let key = (from.to_string(), to.to_string());
    if let Some(&cached) = runner.ctx.exchange_rate_cache.get(&key) {
        return Ok(cached);
    }
    let snap = runner.txn.snapshot();
    let mut reader = CatalogReader::new(runner.storage, snap)?;
    let def = reader.get_exchange_rate(from, to)?.ok_or_else(|| DbError::InvalidValue {
        reason: format!("no exchange rate registered for '{from}' → '{to}'"),
    })?;
    let rate = (def.mantissa, def.scale);
    runner.ctx.exchange_rate_cache.insert(key, rate);
    Ok(rate)
}

// ── CURRENCY_OF(money) ────────────────────────────────────────────────────────

fn currency_of_fn(
    args: &[Expr],
    row: &[Value],
    runner: &mut ExecSubqueryRunner<'_>,
) -> Result<Value, DbError> {
    if args.len() != 1 {
        return Err(DbError::TypeMismatch {
            expected: "CURRENCY_OF(money_expr)".into(),
            got: format!("{} argument(s)", args.len()),
        });
    }
    match crate::eval::eval_with(&args[0], row, runner)? {
        Value::Money(_, _, c) => {
            let s = std::str::from_utf8(&c)
                .map_err(|_| DbError::InvalidValue {
                    reason: "invalid money currency code".into(),
                })?
                .to_string();
            Ok(Value::Text(s))
        }
        Value::Null => Ok(Value::Null),
        other => Err(DbError::TypeMismatch {
            expected: "MONEY value for CURRENCY_OF()".into(),
            got: other.variant_name().to_string(),
        }),
    }
}

// ── AMOUNT_OF(money) ──────────────────────────────────────────────────────────

fn amount_of_fn(
    args: &[Expr],
    row: &[Value],
    runner: &mut ExecSubqueryRunner<'_>,
) -> Result<Value, DbError> {
    if args.len() != 1 {
        return Err(DbError::TypeMismatch {
            expected: "AMOUNT_OF(money_expr)".into(),
            got: format!("{} argument(s)", args.len()),
        });
    }
    match crate::eval::eval_with(&args[0], row, runner)? {
        Value::Money(m, s, _) => Ok(Value::Decimal(m, s)),
        Value::Null => Ok(Value::Null),
        other => Err(DbError::TypeMismatch {
            expected: "MONEY value for AMOUNT_OF()".into(),
            got: other.variant_name().to_string(),
        }),
    }
}
