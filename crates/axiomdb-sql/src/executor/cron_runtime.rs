// Cron job runtime functions: cron_schedule, cron_unschedule, cron_enable, cron_disable.
//
// Included into the executor module (mod.rs) so it shares all imports.
// Each function opens its own auto-commit mini-transaction, following the
// same pattern as sequence_runtime.rs.

fn eval_cron_function(
    name: &str,
    args: &[Expr],
    row: &[Value],
    runner: &mut ExecSubqueryRunner<'_>,
) -> Result<Option<Value>, DbError> {
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "cron_schedule" => cron_schedule_fn(args, row, runner).map(Some),
        "cron_unschedule" => cron_unschedule_fn(args, row, runner).map(Some),
        "cron_enable" => cron_set_enabled_fn(args, row, runner, true).map(Some),
        "cron_disable" => cron_set_enabled_fn(args, row, runner, false).map(Some),
        _ => Ok(None),
    }
}

fn cron_schedule_fn(
    args: &[Expr],
    row: &[Value],
    runner: &mut ExecSubqueryRunner<'_>,
) -> Result<Value, DbError> {
    if args.len() != 3 {
        return Err(DbError::TypeMismatch {
            expected: "cron_schedule(name TEXT, schedule TEXT, command TEXT)".into(),
            got: format!("{} args", args.len()),
        });
    }
    let name = eval_text_arg("cron_schedule", "name", &args[0], row, runner)?;
    let schedule = eval_text_arg("cron_schedule", "schedule", &args[1], row, runner)?;
    let command = eval_text_arg("cron_schedule", "command", &args[2], row, runner)?;

    validate_cron_schedule(&schedule)?;
    if name.is_empty() {
        return Err(DbError::InvalidValue {
            reason: "cron job name must not be empty".into(),
        });
    }
    if command.is_empty() {
        return Err(DbError::InvalidValue {
            reason: "cron job command must not be empty".into(),
        });
    }

    let database = runner.ctx.effective_database().to_string();
    let def = axiomdb_catalog::CronJobDef {
        name: name.clone(),
        schedule,
        command,
        database_name: database,
        enabled: true,
        next_run_ms: 0,
        last_run_ms: 0,
        last_status: String::new(),
    };

    let mut conn = runner.txn.begin()?;
    // Sub-txn: stamp its own id for frame writes; restored to the parent's on drop.
    let _txn_stamp = TxnStamp::new(runner.storage, conn.txn_id);
    let mut writer = CatalogWriter::new(runner.storage, runner.txn, &mut conn)?;
    writer.upsert_cron_job(def)?;
    if let Some(txn_id) = runner.txn.commit_durable(conn, runner.storage)? {
        runner.txn.wal_flush_and_fsync()?;
        runner.txn.advance_committed_single(txn_id);
    }

    Ok(Value::Text(name))
}

fn cron_unschedule_fn(
    args: &[Expr],
    row: &[Value],
    runner: &mut ExecSubqueryRunner<'_>,
) -> Result<Value, DbError> {
    if args.len() != 1 {
        return Err(DbError::TypeMismatch {
            expected: "cron_unschedule(name TEXT)".into(),
            got: format!("{} args", args.len()),
        });
    }
    let name = eval_text_arg("cron_unschedule", "name", &args[0], row, runner)?;

    let mut conn = runner.txn.begin()?;
    // Sub-txn: stamp its own id for frame writes; restored to the parent's on drop.
    let _txn_stamp = TxnStamp::new(runner.storage, conn.txn_id);
    let mut writer = CatalogWriter::new(runner.storage, runner.txn, &mut conn)?;
    let deleted = writer.delete_cron_job(&name)?;
    if let Some(txn_id) = runner.txn.commit_durable(conn, runner.storage)? {
        runner.txn.wal_flush_and_fsync()?;
        runner.txn.advance_committed_single(txn_id);
    }

    Ok(Value::BigInt(i64::from(deleted)))
}

fn cron_set_enabled_fn(
    args: &[Expr],
    row: &[Value],
    runner: &mut ExecSubqueryRunner<'_>,
    enabled: bool,
) -> Result<Value, DbError> {
    let fn_name = if enabled { "cron_enable" } else { "cron_disable" };
    if args.len() != 1 {
        return Err(DbError::TypeMismatch {
            expected: format!("{fn_name}(name TEXT)"),
            got: format!("{} args", args.len()),
        });
    }
    let name = eval_text_arg(fn_name, "name", &args[0], row, runner)?;

    let snap = runner.txn.snapshot();
    let mut reader = CatalogReader::new(runner.storage, snap)?;
    let mut def = reader
        .get_cron_job(&name)?
        .ok_or_else(|| DbError::InvalidValue {
            reason: format!("cron job '{name}' not found"),
        })?;
    def.enabled = enabled;

    let mut conn = runner.txn.begin()?;
    // Sub-txn: stamp its own id for frame writes; restored to the parent's on drop.
    let _txn_stamp = TxnStamp::new(runner.storage, conn.txn_id);
    let mut writer = CatalogWriter::new(runner.storage, runner.txn, &mut conn)?;
    writer.upsert_cron_job(def)?;
    if let Some(txn_id) = runner.txn.commit_durable(conn, runner.storage)? {
        runner.txn.wal_flush_and_fsync()?;
        runner.txn.advance_committed_single(txn_id);
    }

    Ok(Value::BigInt(1))
}

fn eval_text_arg(
    fn_name: &str,
    param: &str,
    arg: &Expr,
    row: &[Value],
    runner: &mut ExecSubqueryRunner<'_>,
) -> Result<String, DbError> {
    let v = crate::eval::eval_with(arg, row, runner)?;
    match v {
        Value::Text(s) => Ok(s),
        other => Err(DbError::TypeMismatch {
            expected: format!("{fn_name}({param}: TEXT)"),
            got: other.to_string(),
        }),
    }
}

/// Validates that `schedule` is a syntactically valid 5-field cron expression
/// without requiring the `cron` crate — does a lightweight structural check.
///
/// Accepts standard 5-field cron: `min hour dom month dow`.
/// Special strings `@hourly`, `@daily`, `@weekly`, `@monthly`, `@yearly`, `@reboot` are accepted.
fn validate_cron_schedule(schedule: &str) -> Result<(), DbError> {
    let s = schedule.trim();
    // Accept @aliases.
    if s.starts_with('@') {
        match s {
            "@hourly" | "@daily" | "@weekly" | "@monthly" | "@yearly" | "@annually"
            | "@midnight" | "@reboot" => return Ok(()),
            _ => {
                return Err(DbError::InvalidValue {
                    reason: format!("unknown cron alias '{s}'; use @hourly, @daily, @weekly, @monthly, @yearly, or a 5-field expression"),
                })
            }
        }
    }
    let fields: Vec<&str> = s.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(DbError::InvalidValue {
            reason: format!(
                "cron schedule must have 5 fields (min hour dom month dow), got {}",
                fields.len()
            ),
        });
    }
    Ok(())
}
