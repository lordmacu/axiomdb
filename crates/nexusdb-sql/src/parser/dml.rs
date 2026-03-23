//! DML statement parsers — SELECT, INSERT, UPDATE, DELETE (Phase 4.4).

use nexusdb_core::error::DbError;

use crate::ast::Stmt;

use super::Parser;

/// Parse a DML statement. Implementation in Phase 4.4.
pub(crate) fn parse_dml(p: &mut Parser) -> Result<Stmt, DbError> {
    Err(DbError::NotImplemented {
        feature: format!("DML parsing for {:?} (Phase 4.4)", p.peek()),
    })
}
