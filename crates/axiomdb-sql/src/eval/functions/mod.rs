use axiomdb_core::error::DbError;
use axiomdb_types::Value;

use crate::expr::Expr;

pub(crate) mod any_all;
pub(crate) mod array;
mod binary;
pub(crate) mod datetime;
mod json;
mod nulls;
mod numeric;
pub(crate) mod range;
mod sql_json_query;
mod string;
mod system;
mod uuid;

pub(crate) use json::{
    execute_jsonpath_owned as execute_jsonpath_owned_public, execute_jsonpath_owned_env,
    parse_jsonpath, parse_jsonpath as parse_jsonpath_public, PassingEnv, PathStep,
};
pub(crate) use sql_json_query::eval_sql_json_query;

pub(super) fn eval_function(name: &str, args: &[Expr], row: &[Value]) -> Result<Value, DbError> {
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "version" | "axiomdb_version" | "current_user" | "user" | "session_user"
        | "system_user" | "current_database" | "database" | "current_schema" | "schema"
        | "connection_id" | "row_count" | "last_insert_id" | "lastval" | "found_rows"
        | "nextval" | "currval" => system::eval(lower.as_str(), args, row),

        "coalesce" | "ifnull" | "nvl" | "nullif" | "isnull" | "if" | "iff" | "typeof"
        | "pg_typeof" | "to_char" | "str" | "tostring" => nulls::eval(lower.as_str(), args, row),

        "abs" | "ceil" | "ceiling" | "floor" | "round" | "pow" | "power" | "sqrt" | "mod"
        | "sign" | "pi" | "exp" | "log" | "log2" | "log10" | "sin" | "cos" | "tan" | "asin"
        | "acos" | "atan" | "atan2" | "cot" | "radians" | "degrees" | "truncate" | "trunc"
        | "rand" | "random" | "greatest" | "least" => numeric::eval(lower.as_str(), args, row),

        // Range constructor functions (Phase 20.13)
        "int4range" | "int8range" | "numrange" | "daterange" | "tsrange"
        | "isempty" | "lowerinc" | "upperinc" | "lowerbnd" | "upperbnd" => {
            range::eval(lower.as_str(), args, row)
        }

        // `lower` and `upper` are overloaded: string functions AND range bound accessors.
        // Dispatch based on the runtime type of the first argument.
        "lower" | "lcase" | "upper" | "ucase" if !args.is_empty() => {
            // Evaluate first arg to check if it's a range
            let first = crate::eval::eval(&args[0], row)?;
            if matches!(first, Value::Range(_)) {
                // Re-eval via range module (it will re-eval args internally)
                range::eval(lower.as_str(), args, row)
            } else {
                string::eval(lower.as_str(), args, row)
            }
        }

        "length" | "char_length" | "character_length" | "len" | "octet_length" | "byte_length"
        | "upper" | "ucase" | "lower" | "lcase" | "trim" | "ltrim" | "rtrim" | "substr"
        | "substring" | "mid" | "concat" | "concat_ws" | "repeat" | "replicate" | "replace"
        | "reverse" | "left" | "right" | "lpad" | "rpad" | "locate" | "position" | "instr"
        | "ascii" | "char" | "chr" | "space" | "strcmp"
        // 4.19j — additional string functions
        | "nvl2" | "insert" | "ord" | "soundex" | "format" | "bin" | "oct" | "hex" | "unhex"
        | "quote" | "field" | "elt" => string::eval(lower.as_str(), args, row),

        // Phase 20.15 — PostgreSQL regex scalar functions
        "regexp_like" => string::eval_regexp_like(args, row),
        "regexp_replace" => string::eval_regexp_replace(args, row),

        "now" | "current_timestamp" | "getdate" | "sysdate" | "current_date" | "curdate"
        | "today" | "unix_timestamp" | "year" | "month" | "day" | "hour" | "minute" | "second"
        | "datediff" | "date_format" | "str_to_date" | "find_in_set"
        // 4.19i — UTC / epoch / interval functions
        | "utc_timestamp" | "utc_date" | "utc_time" | "from_unixtime" | "convert_tz"
        | "adddate" | "date_add" | "subdate" | "date_sub" | "timestampdiff" | "makedate"
        | "dayofweek" | "day_of_month" | "dayofmonth" | "dayofyear" | "weekday" | "quarter"
        | "week" | "weekofyear" | "yearweek" | "last_day" | "date" | "time" | "timediff" => {
            datetime::eval(lower.as_str(), args, row)
        }

        "from_base64" | "to_base64" | "encode" | "decode" | "mime_type" => {
            binary::eval(lower.as_str(), args, row)
        }

        "json_extract" | "json_set" | "json_remove" | "json_keys" | "json_valid"
        | "json_type" | "json_merge_patch" | "json_contains" | "json_overlaps"
        | "json_array_length" | "json_depth" | "json_pretty"
        | "to_jsonb" | "jsonb"
        | "json_path_exists" | "json_path_query" | "json_path_query_first"
        | "jsonb_path_exists" | "jsonb_path_query" | "jsonb_path_query_first"
        | "jsonb_path_query_array" | "jsonb_path_match"
        // Phase 11.18a: function-style aliases of the new operators so SQL
        // portable across MySQL / MariaDB / DuckDB (which lack ?, <@, ||
        // operator syntax) can still drive the same machinery.
        | "jsonb_exists" | "jsonb_contained" | "jsonb_concat"
        | "jsonb_delete_key" | "jsonb_delete_index"
        // Phase 11.22a: PG JSONB mutation functions + MySQL siblings that
        // were not yet implemented. All share the same path-normalizer and
        // a flag-driven `set_path_ext` core so semantics stay consistent
        // while the per-function flag combination preserves vendor behavior.
        | "jsonb_set" | "jsonb_set_lax" | "jsonb_insert" | "jsonb_delete_path"
        | "json_equal" | "json_scalar" | "json_serialize"
        | "json_schema_valid" | "json_schema_validation_report"
        | "json_quote" | "json_unquote" | "json_length" | "json_storage_size"
        | "json_array_append" | "json_array_insert"
        | "json_array" | "json_object" | "json_merge_preserve" | "json_merge"
        | "json_contains_path" | "json_search" | "json_transform"
        | "json_dataguide"
        | "json_insert" | "json_replace"
        // Phase 11.25c — PG construction helpers + to_json alias.
        | "jsonb_build_object" | "jsonb_build_array" | "to_json"
        // Phase 11.25d — PG + MySQL mutator/introspection completion.
        | "jsonb_strip_nulls" | "json_storage_free" => {
            json::eval(lower.as_str(), args, row)
        }

        // Phase 11.6: MATCH(text_col, 'query terms') → relevance score (f64).
        // Returns 0.0 if no match, >0.0 if terms found. Higher = more matches.
        // This is the eval-only path; index-accelerated search uses the planner.
        "match" => {
            if args.len() < 2 {
                return Err(DbError::TypeMismatch {
                    expected: "MATCH(col, query): 2 args".into(),
                    got: args.len().to_string(),
                });
            }
            let text_val = crate::eval::eval(&args[0], row)?;
            let query_val = crate::eval::eval(&args[1], row)?;
            let (text, query) = match (text_val, query_val) {
                (Value::Null, _) | (_, Value::Null) => return Ok(Value::Real(0.0)),
                (Value::Text(t) | Value::Json(t), Value::Text(q) | Value::Json(q)) => (t, q),
                (t, q) => (t.to_string(), q.to_string()),
            };
            // Phase 11.7: parse advanced FTS query (boolean, phrase, prefix).
            let clauses = crate::fts_query::parse_fts_query(&query);
            let score = crate::fts_query::evaluate_fts(&clauses, &text);
            Ok(Value::Real(score))
        }

        "gen_random_uuid" | "uuid_generate_v4" | "random_uuid" | "newid" | "uuid_generate_v7"
        | "uuid7" | "is_valid_uuid" | "is_uuid" => uuid::eval(lower.as_str(), args, row),

        // Phase 20.4 Step 6: array functions
        "array_length" | "array_lower" | "array_upper" | "array_ndims" | "array_dims"
        | "cardinality" | "array_append" | "array_prepend" | "array_cat"
        | "array_remove" | "array_replace" | "array_position" | "array_to_string"
        | "string_to_array" | "unnest" => array::eval(lower.as_str(), args, row),

        // Cron job functions (Phase 22b.1) — handled by cron_runtime.rs in the executor;
        // reaching here means we are in a pure-eval context without storage access.
        "cron_schedule" | "cron_unschedule" | "cron_enable" | "cron_disable" => {
            Err(DbError::NotImplemented {
                feature: format!(
                    "'{name}' requires a storage context — call from a SELECT, not a WHERE clause"
                ),
            })
        }

        // Phase 20.17: MONEY pure functions (no storage needed).
        // convert_currency is handled by money_runtime.rs (needs catalog access).
        "money" => eval_money_constructor_pure(args, row),
        "currency_of" => eval_currency_of_pure(args, row),
        "amount_of" => eval_amount_of_pure(args, row),

        _ => Err(DbError::NotImplemented {
            feature: format!("function '{name}' — add to Phase 4.19 eval.rs"),
        }),
    }
}

// ── Pure MONEY helpers (Phase 20.17) ─────────────────────────────────────────
// These mirror money_runtime.rs but work without storage context.

fn eval_money_constructor_pure(args: &[Expr], row: &[Value]) -> Result<Value, DbError> {
    use axiomdb_types::{coerce, CoercionMode, DataType};
    if args.len() != 2 {
        return Err(DbError::TypeMismatch {
            expected: "MONEY(amount, currency_code)".into(),
            got: format!("{} argument(s)", args.len()),
        });
    }
    let amount = crate::eval::eval(&args[0], row)?;
    let currency_raw = crate::eval::eval(&args[1], row)?;

    let (mantissa, scale) = match amount {
        Value::Decimal(m, s) => (m, s),
        Value::Int(n) => (n as i128, 0u8),
        Value::BigInt(n) => (n as i128, 0u8),
        Value::Real(f) => {
            let s = 6u8;
            let m = (f * 1_000_000.0).round() as i128;
            (m, s)
        }
        Value::Text(ref s) => match coerce(
            Value::Text(s.clone()),
            DataType::Decimal,
            CoercionMode::Strict,
        ) {
            Ok(Value::Decimal(m, sc)) => (m, sc),
            _ => {
                return Err(DbError::TypeMismatch {
                    expected: "numeric amount for MONEY()".into(),
                    got: s.clone(),
                })
            }
        },
        Value::Null => return Ok(Value::Null),
        other => {
            return Err(DbError::TypeMismatch {
                expected: "numeric amount for MONEY()".into(),
                got: other.variant_name().to_string(),
            })
        }
    };

    let currency_code = match currency_raw {
        Value::Text(s) => s.to_ascii_uppercase(),
        other => {
            return Err(DbError::TypeMismatch {
                expected: "currency code string for MONEY()".into(),
                got: other.variant_name().to_string(),
            })
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

fn eval_currency_of_pure(args: &[Expr], row: &[Value]) -> Result<Value, DbError> {
    if args.len() != 1 {
        return Err(DbError::TypeMismatch {
            expected: "CURRENCY_OF(money_expr)".into(),
            got: format!("{} argument(s)", args.len()),
        });
    }
    match crate::eval::eval(&args[0], row)? {
        Value::Money(_, _, c) => {
            let s = std::str::from_utf8(&c)
                .map_err(|_| DbError::InvalidValue {
                    reason: "invalid currency code".into(),
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

fn eval_amount_of_pure(args: &[Expr], row: &[Value]) -> Result<Value, DbError> {
    if args.len() != 1 {
        return Err(DbError::TypeMismatch {
            expected: "AMOUNT_OF(money_expr)".into(),
            got: format!("{} argument(s)", args.len()),
        });
    }
    match crate::eval::eval(&args[0], row)? {
        Value::Money(m, s, _) => Ok(Value::Decimal(m, s)),
        Value::Null => Ok(Value::Null),
        other => Err(DbError::TypeMismatch {
            expected: "MONEY value for AMOUNT_OF()".into(),
            got: other.variant_name().to_string(),
        }),
    }
}
