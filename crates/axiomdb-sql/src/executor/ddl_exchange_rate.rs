// ── Exchange rate DDL (Phase 20.17) ───────────────────────────────────────────

fn execute_create_exchange_rate(
    stmt: crate::ast::CreateExchangeRateStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
) -> Result<QueryResult, DbError> {
    use axiomdb_types::{coerce, CoercionMode, DataType, Value};

    let from = stmt.from_currency.to_ascii_uppercase();
    let to = stmt.to_currency.to_ascii_uppercase();

    for code in &[&from, &to] {
        if code.len() != 3 || !code.chars().all(|c| c.is_ascii_alphabetic()) {
            return Err(DbError::InvalidValue {
                reason: format!("currency code '{code}' must be 3 ASCII letters (ISO 4217)"),
            });
        }
    }

    let (mantissa, scale) = match coerce(
        Value::Text(stmt.rate_str.clone()),
        DataType::Decimal,
        CoercionMode::Strict,
    ) {
        Ok(Value::Decimal(m, s)) => (m, s),
        _ => {
            return Err(DbError::InvalidValue {
                reason: format!("invalid exchange rate '{}'", stmt.rate_str),
            });
        }
    };

    if mantissa <= 0 {
        return Err(DbError::InvalidValue {
            reason: "exchange rate must be a positive number".into(),
        });
    }

    let def = axiomdb_catalog::ExchangeRateDef {
        from_currency: from,
        to_currency: to,
        mantissa,
        scale,
    };
    let mut writer = CatalogWriter::new(storage, txn, conn_txn)?;
    writer.upsert_exchange_rate(def)?;
    Ok(QueryResult::Empty)
}

fn execute_drop_exchange_rate(
    stmt: crate::ast::DropExchangeRateStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
) -> Result<QueryResult, DbError> {
    let from = stmt.from_currency.to_ascii_uppercase();
    let to = stmt.to_currency.to_ascii_uppercase();
    let mut writer = CatalogWriter::new(storage, txn, conn_txn)?;
    let found = writer.delete_exchange_rate(&from, &to)?;
    if !found && !stmt.if_exists {
        return Err(DbError::InvalidValue {
            reason: format!("exchange rate '{from}' TO '{to}' does not exist"),
        });
    }
    Ok(QueryResult::Empty)
}
