fn eval_sequence_function(
    name: &str,
    args: &[Expr],
    row: &[Value],
    runner: &mut ExecSubqueryRunner<'_>,
) -> Result<Option<Value>, DbError> {
    let lower = name.to_ascii_lowercase();
    if lower != "nextval" && lower != "currval" {
        return Ok(None);
    }
    if args.len() != 1 {
        return Err(DbError::TypeMismatch {
            expected: format!("{lower}(text): 1 arg"),
            got: args.len().to_string(),
        });
    }
    let raw = crate::eval::eval_with(&args[0], row, runner)?;
    let Value::Text(seq_name) = raw else {
        return Err(DbError::TypeMismatch {
            expected: format!("{lower}(text)"),
            got: raw.to_string(),
        });
    };
    let (schema, name) = resolve_sequence_name(&seq_name, runner.ctx.default_create_schema());
    let key = sequence_currval_key(&schema, &name);

    if lower == "currval" {
        let value =
            runner
                .ctx
                .sequence_currvals
                .get(&key)
                .copied()
                .ok_or_else(|| DbError::InvalidValue {
                    reason: format!(
                        "currval of sequence '{}.{}' is not yet defined in this session",
                        schema, name
                    ),
                })?;
        return Ok(Some(Value::BigInt(value)));
    }

    let value = next_sequence_value(runner.storage, runner.txn, &schema, &name)?;
    runner.ctx.sequence_currvals.insert(key, value);
    Ok(Some(Value::BigInt(value)))
}

fn resolve_sequence_name(raw: &str, default_schema: &str) -> (String, String) {
    let stripped = raw.trim().trim_matches('"').trim_matches('`');
    if let Some((schema, name)) = stripped.rsplit_once('.') {
        (
            schema.trim_matches('"').trim_matches('`').to_string(),
            name.trim_matches('"').trim_matches('`').to_string(),
        )
    } else {
        (default_schema.to_string(), stripped.to_string())
    }
}

fn sequence_currval_key(schema: &str, name: &str) -> String {
    format!("{}.{}", schema.to_ascii_lowercase(), name.to_ascii_lowercase())
}

fn next_sequence_value(
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    schema: &str,
    name: &str,
) -> Result<i64, DbError> {
    let mut conn = txn.begin()?;
    let snap = txn.active_snapshot(&conn);
    let mut reader = CatalogReader::new(storage, snap)?;
    let mut def = reader
        .get_sequence(schema, name)?
        .ok_or_else(|| DbError::InvalidValue {
            reason: format!("sequence '{}.{}' not found", schema, name),
        })?;

    let value = advance_sequence_value(&mut def)?;
    let mut writer = CatalogWriter::new(storage, txn, &mut conn)?;
    writer.replace_sequence_state(def)?;
    if let Some(txn_id) = txn.commit(conn)? {
        txn.wal_flush_and_fsync()?;
        txn.advance_committed_single(txn_id);
    }
    Ok(value)
}

fn advance_sequence_value(def: &mut axiomdb_catalog::SequenceDef) -> Result<i64, DbError> {
    if !def.is_called {
        def.is_called = true;
        def.last_value = def.start_value;
        return Ok(def.start_value);
    }

    let Some(next) = def.last_value.checked_add(def.increment) else {
        return sequence_exhausted(def);
    };
    if next > def.max_value {
        if def.cycle {
            def.last_value = def.min_value;
            return Ok(def.last_value);
        }
        return sequence_exhausted(def);
    }
    if next < def.min_value {
        if def.cycle {
            def.last_value = def.max_value;
            return Ok(def.last_value);
        }
        return sequence_exhausted(def);
    }
    def.last_value = next;
    Ok(next)
}

fn sequence_exhausted(def: &axiomdb_catalog::SequenceDef) -> Result<i64, DbError> {
    Err(DbError::InvalidValue {
        reason: format!(
            "nextval: reached {} value of sequence \"{}\"",
            if def.increment >= 0 { "maximum" } else { "minimum" },
            def.name
        ),
    })
}
