//! JSON error payload for `SET error_format = 'json'`.
//!
//! When a client sets `error_format = 'json'` in the session, ERR packets
//! carry a JSON string instead of a plain text message. This lets clients that
//! understand the format (ORMs, dashboards, CLIs) parse structured error fields
//! without screen-scraping the message string.
//!
//! The JSON structure mirrors PostgreSQL's `ErrorResponse` fields while staying
//! wire-compatible with MySQL (the ERR packet's message field is just a string).

use axiomdb_core::error::DbError;
use axiomdb_core::error_response::ErrorResponse;
use serde::Serialize;

use super::error::dberror_to_mysql;

// ── JSON payload ──────────────────────────────────────────────────────────────

/// Serializable error payload for JSON error format.
///
/// Lives entirely in `axiomdb-network`; does not add serde to `axiomdb-core`.
#[derive(Serialize)]
struct JsonErrorPayload<'a> {
    /// MySQL error code (e.g. 1064 for syntax errors, 1062 for unique violations).
    code: u16,
    /// 5-character SQLSTATE code (e.g. "42000", "23000").
    sqlstate: &'a str,
    /// Severity string ("ERROR", "WARNING", "NOTICE").
    severity: &'a str,
    /// Short human-readable error message. Does NOT include the visual snippet —
    /// clients should use `position` to highlight the relevant token in their UI.
    message: &'a str,
    /// Extended detail about the error (e.g. offending value for UniqueViolation).
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<&'a str>,
    /// Actionable hint for how to fix the error.
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<&'a str>,
    /// 0-based byte offset of the unexpected token in the SQL string.
    /// `null` for errors that have no associated SQL position.
    position: Option<usize>,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Builds a JSON string suitable for use as the message in a MySQL ERR packet.
///
/// The `code` is the MySQL error number (from [`dberror_to_mysql`]).
/// Uses [`ErrorResponse`] for structured fields (detail, hint, position).
pub fn build_json_error(e: &DbError, sql: Option<&str>) -> String {
    let me = dberror_to_mysql(e, sql);
    let resp = ErrorResponse::from_error(e);
    let sev = resp.severity.to_string();
    let payload = JsonErrorPayload {
        code: me.code,
        sqlstate: &resp.sqlstate,
        severity: &sev,
        message: &resp.message,
        detail: resp.detail.as_deref(),
        hint: resp.hint.as_deref(),
        position: resp.position,
    };
    serde_json::to_string(&payload).unwrap_or_else(|_| {
        // Infallible in practice, but provide a safe fallback.
        format!(
            r#"{{"code":{},"sqlstate":"{}","message":"{}"}}"#,
            me.code,
            resp.sqlstate,
            resp.message.replace('"', "\\\""),
        )
    })
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axiomdb_core::error::DbError;

    #[test]
    fn json_parse_error_has_position() {
        let err = DbError::ParseError {
            message: "unexpected token 'FORM'".into(),
            position: Some(9),
        };
        let json = build_json_error(&err, Some("SELECT * FORM t"));
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(v["code"], 1064);
        assert_eq!(v["sqlstate"], "42601");
        assert_eq!(v["severity"], "ERROR");
        assert_eq!(v["position"], 9);
        // message should NOT contain the visual snippet
        let msg = v["message"].as_str().unwrap();
        assert!(
            !msg.contains('^'),
            "JSON message should be clean, got: {msg}"
        );
    }

    #[test]
    fn json_unique_violation_has_detail() {
        let err = DbError::UniqueViolation {
            index_name: "users_email_idx".into(),
            value: Some("alice@example.com".into()),
        };
        let json = build_json_error(&err, None);
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(v["code"], 1062);
        assert_eq!(v["sqlstate"], "23505");
        let detail = v["detail"].as_str().unwrap();
        assert!(
            detail.contains("alice@example.com"),
            "detail should contain value: {detail}"
        );
    }

    #[test]
    fn json_error_is_valid_json() {
        let err = DbError::TableNotFound {
            name: "orders".into(),
        };
        let json = build_json_error(&err, None);
        let v: serde_json::Value = serde_json::from_str(&json).expect("must be valid JSON");
        assert!(v["message"].as_str().is_some());
        assert!(v["sqlstate"].as_str().is_some());
    }
}
