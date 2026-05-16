// ── Holiday calendar DDL (Phase 20.16) ───────────────────────────────────────

/// Parses an ISO-8601 date string `"YYYY-MM-DD"` into days since Unix epoch.
///
/// Returns a [`DbError::InvalidValue`] if the string is not a valid date.
fn parse_holiday_date(s: &str) -> Result<i32, DbError> {
    use chrono::NaiveDate;
    let date = NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| DbError::InvalidValue {
        reason: format!("invalid holiday date '{s}': expected YYYY-MM-DD format"),
    })?;
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
    let days = date.signed_duration_since(epoch).num_days();
    i32::try_from(days).map_err(|_| DbError::InvalidValue {
        reason: format!("holiday date '{s}' is out of the representable range"),
    })
}

fn execute_create_holiday_calendar(
    stmt: crate::ast::CreateHolidayCalendarStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
) -> Result<QueryResult, DbError> {
    let country_code = stmt.country_code.to_ascii_uppercase();
    if country_code.is_empty() || country_code.len() > 16 {
        return Err(DbError::InvalidValue {
            reason: "country code must be 1–16 characters".into(),
        });
    }

    // Parse, sort, and deduplicate holiday dates.
    let mut holidays: Vec<i32> = stmt
        .holidays
        .iter()
        .map(|s| parse_holiday_date(s))
        .collect::<Result<Vec<_>, _>>()?;
    holidays.sort_unstable();
    holidays.dedup();

    let def = axiomdb_catalog::HolidayCalendarDef {
        country_code,
        holidays,
    };
    let mut writer = CatalogWriter::new(storage, txn, conn_txn)?;
    writer.upsert_holiday_calendar(def)?;
    Ok(QueryResult::Empty)
}

fn execute_drop_holiday_calendar(
    stmt: crate::ast::DropHolidayCalendarStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
) -> Result<QueryResult, DbError> {
    let country_code = stmt.country_code.to_ascii_uppercase();
    let mut writer = CatalogWriter::new(storage, txn, conn_txn)?;
    let found = writer.delete_holiday_calendar(&country_code)?;
    if !found && !stmt.if_exists {
        return Err(DbError::InvalidValue {
            reason: format!("holiday calendar '{country_code}' does not exist"),
        });
    }
    Ok(QueryResult::Empty)
}
