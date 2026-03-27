use axiomdb_core::error::DbError;
use axiomdb_types::Value;

use crate::expr::Expr;

pub(super) fn eval(name: &str, _args: &[Expr], _row: &[Value]) -> Result<Value, DbError> {
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
        "last_insert_id" | "lastval" => {
            let id = crate::executor::last_insert_id_value();
            Ok(Value::BigInt(id as i64))
        }

        _ => unreachable!("dispatcher routed unsupported system function"),
    }
}
