use axiomdb_core::error::DbError;
use axiomdb_types::Value;

use crate::expr::Expr;

pub(super) fn eval(name: &str, args: &[Expr], row: &[Value]) -> Result<Value, DbError> {
    match name {
        // ── System functions (4.13) ─────────────────────────────────────────
        "version" | "axiomdb_version" => Ok(Value::Text("AxiomDB 0.1.0".into())),
        "current_user" | "user" | "session_user" | "system_user" => {
            Ok(Value::Text("axiomdb".into()))
        }
        "current_database" | "database" => Ok(Value::Text("main".into())),
        "current_schema" | "schema" => Ok(Value::Text("public".into())),
        "connection_id" => Ok(Value::BigInt(1)),
        "row_count" => Ok(Value::BigInt(0)),

        // ── LAST_INSERT_ID / lastval (4.14) ──────────────────────────────────
        // 1-arg form (4.14b): LAST_INSERT_ID(expr) evaluates expr, stores it,
        // and returns it — used for manual sequence tracking patterns.
        "last_insert_id" | "lastval" => {
            if args.is_empty() {
                let id = crate::executor::last_insert_id_value();
                Ok(Value::BigInt(id as i64))
            } else {
                let v = crate::eval::eval(&args[0], row)?;
                let id = match &v {
                    Value::Int(n) => *n as u64,
                    Value::BigInt(n) => *n as u64,
                    _ => 0,
                };
                crate::executor::set_last_insert_id(id);
                Ok(Value::BigInt(id as i64))
            }
        }

        // ── FOUND_ROWS() (4.5e) ──────────────────────────────────────────────
        // Returns the number of rows that the last SELECT would have returned
        // without its LIMIT clause, when SQL_CALC_FOUND_ROWS was used.
        // Without SQL_CALC_FOUND_ROWS, returns the same as ROW_COUNT().
        "found_rows" => Ok(Value::BigInt(crate::executor::found_rows_value() as i64)),

        _ => unreachable!("dispatcher routed unsupported system function"),
    }
}
