use axiomdb_core::error::DbError;
use axiomdb_types::Value;

use crate::expr::Expr;

mod binary;
pub(crate) mod datetime;
mod nulls;
mod numeric;
mod string;
mod system;
mod uuid;

pub(super) fn eval_function(name: &str, args: &[Expr], row: &[Value]) -> Result<Value, DbError> {
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "version" | "axiomdb_version" | "current_user" | "user" | "session_user"
        | "system_user" | "current_database" | "database" | "current_schema" | "schema"
        | "connection_id" | "row_count" | "last_insert_id" | "lastval" | "found_rows" => {
            system::eval(lower.as_str(), args, row)
        }

        "coalesce" | "ifnull" | "nvl" | "nullif" | "isnull" | "if" | "iff" | "typeof"
        | "pg_typeof" | "to_char" | "str" | "tostring" => nulls::eval(lower.as_str(), args, row),

        "abs" | "ceil" | "ceiling" | "floor" | "round" | "pow" | "power" | "sqrt" | "mod"
        | "sign" | "pi" | "exp" | "log" | "log2" | "log10" | "sin" | "cos" | "tan" | "asin"
        | "acos" | "atan" | "atan2" | "cot" | "radians" | "degrees" | "truncate" | "trunc"
        | "rand" | "random" | "greatest" | "least" => numeric::eval(lower.as_str(), args, row),

        "length" | "char_length" | "character_length" | "len" | "octet_length" | "byte_length"
        | "upper" | "ucase" | "lower" | "lcase" | "trim" | "ltrim" | "rtrim" | "substr"
        | "substring" | "mid" | "concat" | "concat_ws" | "repeat" | "replicate" | "replace"
        | "reverse" | "left" | "right" | "lpad" | "rpad" | "locate" | "position" | "instr"
        | "ascii" | "char" | "chr" | "space" | "strcmp"
        // 4.19j — additional string functions
        | "nvl2" | "insert" | "ord" | "soundex" | "format" | "bin" | "oct" | "hex" | "unhex"
        | "quote" | "field" | "elt" => string::eval(lower.as_str(), args, row),

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

        "gen_random_uuid" | "uuid_generate_v4" | "random_uuid" | "newid" | "uuid_generate_v7"
        | "uuid7" | "is_valid_uuid" | "is_uuid" => uuid::eval(lower.as_str(), args, row),

        _ => Err(DbError::NotImplemented {
            feature: format!("function '{name}' — add to Phase 4.19 eval.rs"),
        }),
    }
}
