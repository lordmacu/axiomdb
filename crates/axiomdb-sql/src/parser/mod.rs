//! SQL parser — recursive descent over a [`SpannedToken`] stream.
//!
//! Entry point: [`parse`].
//!
//! ## Internal modules
//!
//! - [`expr`] — expression sub-parser (literals, comparisons, AND/OR/NOT)
//! - [`ddl`]  — DDL statement parsers (CREATE/DROP TABLE/INDEX)
//! - [`dml`]  — DML statement parsers (Phase 4.4)

pub(crate) mod ddl;
pub(crate) mod dml;
pub(crate) mod expr;
pub(crate) mod json_table;
pub(crate) mod sql_json_common;

use axiomdb_core::error::DbError;

use axiomdb_types::Value;

use crate::{
    ast::{SetStmt, SetValue, Stmt, TableRef},
    lexer::{Span, SpannedToken, Token},
    session::SqlModeFlags,
};

// ── Public entry point ────────────────────────────────────────────────────────

/// Parse a single SQL statement from `input`.
///
/// Tokenizes `input` (forwarding `max_bytes` for the 4.2b size check), then
/// parses the token stream into a [`Stmt`].
///
/// # Errors
/// - [`DbError::ParseError`] — input too long, unrecognized character,
///   unexpected token, missing token, or identifier > 64 characters.
pub fn parse(input: &str, max_bytes: Option<usize>) -> Result<Stmt, DbError> {
    parse_with_sql_mode(input, max_bytes, SqlModeFlags::default())
}

/// Parse a single SQL statement from `input` with parser-affecting SQL mode flags.
pub fn parse_with_sql_mode(
    input: &str,
    max_bytes: Option<usize>,
    sql_mode: SqlModeFlags,
) -> Result<Stmt, DbError> {
    // 4.1f: expand MySQL version-conditional comments `/*!NNNNN SQL*/` before
    // tokenizing. If the input contains no `/*!`, no allocation is made.
    let expanded;
    let effective = match crate::lexer::expand_version_comments(input) {
        Some(s) => {
            expanded = s;
            expanded.as_str()
        }
        None => input,
    };
    let tokens = crate::lexer::tokenize_with_sql_mode(effective, max_bytes, sql_mode)?;
    let mut p = Parser::new(&tokens, effective);
    let stmt = p.parse_stmt()?;

    // After parsing, only Eof (or Semicolon+Eof) should remain.
    // For now, require exactly Eof (single-statement mode).
    // Multi-statement support comes in Phase 4.5.
    p.eat(&Token::Semicolon);
    if p.peek() != &Token::Eof {
        return Err(DbError::ParseError {
            message: format!("unexpected token {:?} after statement", p.peek(),),
            position: Some(p.current_pos()),
        });
    }

    Ok(stmt)
}

/// Parses a single SQL expression from `input`.
///
/// Used to re-evaluate CHECK constraint expressions stored in `axiom_constraints`
/// (Phase 4.22b). Returns `DbError::ParseError` if `input` is not a valid expression.
pub fn parse_expr_only(input: &str) -> Result<crate::expr::Expr, DbError> {
    parse_expr_only_with_sql_mode(input, SqlModeFlags::default())
}

/// Parses a single SQL expression from `input` with parser-affecting SQL mode flags.
pub fn parse_expr_only_with_sql_mode(
    input: &str,
    sql_mode: SqlModeFlags,
) -> Result<crate::expr::Expr, DbError> {
    let tokens = crate::lexer::tokenize_with_sql_mode(input, None, sql_mode)?;
    let mut p = Parser::new(&tokens, input);
    let e = expr::parse_expr(&mut p)?;
    Ok(e)
}

// ── Parser struct ─────────────────────────────────────────────────────────────

/// Recursive descent parser over a slice of [`SpannedToken`]s.
///
/// The lifetime `'src` is tied to the original SQL input string.
pub(crate) struct Parser<'src> {
    input: &'src str,
    tokens: &'src [SpannedToken<'src>],
    pos: usize,
    /// Parameter index counter for `?` placeholders in prepared statement templates.
    /// Incremented each time `Token::Question` is consumed via `parse_atom`.
    pub(crate) param_count: usize,
    /// `true` while parsing the assignment list of
    /// `INSERT ... ON DUPLICATE KEY UPDATE`. The expression parser uses this
    /// to recognize `VALUES(col)` as the MySQL pseudo-function referring to
    /// the proposed row; outside ODKU it stays a normal identifier / call.
    pub(crate) in_odku_assignment: bool,
    /// `true` while parsing PostgreSQL `ON CONFLICT DO UPDATE` expressions.
    /// The expression parser uses this to recognize `EXCLUDED.col` as the
    /// proposed-row value rather than a normal qualified column reference.
    pub(crate) in_on_conflict_expr: bool,
}

impl<'src> Parser<'src> {
    pub(crate) fn new(tokens: &'src [SpannedToken<'src>], input: &'src str) -> Self {
        Self {
            input,
            tokens,
            pos: 0,
            param_count: 0,
            in_odku_assignment: false,
            in_on_conflict_expr: false,
        }
    }

    // ── Peek helpers ──────────────────────────────────────────────────────────

    /// Current token without advancing. Returns `&Token::Eof` at end of stream.
    pub(crate) fn peek(&self) -> &Token<'src> {
        self.tokens
            .get(self.pos)
            .map(|st| &st.token)
            .unwrap_or(&Token::Eof)
    }

    /// Look-ahead by `offset` positions. Returns `&Token::Eof` past end.
    #[allow(dead_code)]
    pub(crate) fn peek_at(&self, offset: usize) -> &Token<'src> {
        self.tokens
            .get(self.pos + offset)
            .map(|st| &st.token)
            .unwrap_or(&Token::Eof)
    }

    /// Byte position of the current token (for error messages).
    pub(crate) fn current_pos(&self) -> usize {
        self.tokens
            .get(self.pos)
            .map(|st| st.span.start)
            .unwrap_or(0)
    }

    /// Span of the current token.
    #[allow(dead_code)]
    pub(crate) fn current_span(&self) -> Span {
        self.tokens
            .get(self.pos)
            .map(|st| st.span)
            .unwrap_or(Span { start: 0, end: 0 })
    }

    pub(crate) fn previous_end(&self) -> usize {
        self.pos
            .checked_sub(1)
            .and_then(|idx| self.tokens.get(idx))
            .map(|st| st.span.end)
            .unwrap_or(0)
    }

    pub(crate) fn slice_sql(&self, start: usize, end: usize) -> String {
        self.input.get(start..end).unwrap_or("").trim().to_string()
    }

    // ── Advance helpers ───────────────────────────────────────────────────────

    /// Consume current token and advance. Panics only if already at Eof
    /// (should not happen — callers must check `peek() != Eof` first).
    pub(crate) fn advance(&mut self) -> &SpannedToken<'src> {
        let st = &self.tokens[self.pos];
        self.pos += 1;
        st
    }

    /// Consume if the current token equals `expected`; return error otherwise.
    pub(crate) fn expect(&mut self, expected: &Token<'_>) -> Result<(), DbError> {
        if self.peek() == expected {
            self.pos += 1;
            Ok(())
        } else {
            Err(DbError::ParseError {
                message: format!("expected {:?} but found {:?}", expected, self.peek(),),
                position: Some(self.current_pos()),
            })
        }
    }

    /// Consume if current token equals `expected`; return `false` if not.
    pub(crate) fn eat(&mut self, expected: &Token<'_>) -> bool {
        if self.peek() == expected {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Consume the current token if it is an identifier matching `keyword`
    /// case-insensitively; return `true` if consumed, `false` otherwise.
    pub(crate) fn eat_ident_ci(&mut self, keyword: &str) -> bool {
        match self.peek() {
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case(keyword) => {
                self.pos += 1;
                true
            }
            _ => false,
        }
    }

    /// Parse the current token as a string literal and return its value.
    /// Returns an error if the current token is not a string literal.
    pub(crate) fn parse_string_literal(&mut self) -> Result<String, DbError> {
        match self.peek().clone() {
            Token::StringLit(s) => {
                self.pos += 1;
                Ok(s.clone())
            }
            other => Err(DbError::ParseError {
                message: format!("expected string literal, found {other:?}"),
                position: Some(self.current_pos()),
            }),
        }
    }

    // ── Identifier helpers ─────────────────────────────────────────────────────

    /// Parse an unquoted or quoted identifier.
    ///
    /// Converts the zero-copy `&'src str` to an owned `String` exactly once.
    /// Validates the 64-character limit (4.3d).
    pub(crate) fn parse_identifier(&mut self) -> Result<String, DbError> {
        let pos = self.current_pos();
        let name = match self.peek().clone() {
            Token::Ident(s) | Token::QuotedIdent(s) => {
                self.pos += 1;
                s.to_string() // &'src str → String: the one allocation per identifier
            }
            Token::DqIdent(s) => {
                self.pos += 1;
                s
            }
            // Allow certain keywords to be used as identifiers (unreserved words).
            Token::Key
            | Token::Index
            | Token::Tables
            | Token::Desc
            | Token::Set
            | Token::Action
            | Token::Names
            | Token::Autocommit
            | Token::Work
            | Token::Read
            | Token::Only
            | Token::Write
            | Token::Global
            | Token::Session
            | Token::Local
            | Token::Lock
            | Token::Unlock
            | Token::Flush
            | Token::Kill
            | Token::Query
            | Token::Connection
            | Token::Regexp
            | Token::Rlike
            | Token::Xor
            | Token::IntDiv
            // DML keywords that double as MySQL built-in function names.
            | Token::Truncate  // TRUNCATE(x, d) — numeric rounding function
            | Token::Insert    // INSERT(str, pos, len, newstr) — string replacement
            | Token::Merge
            => {
                let tok = self.advance();
                keyword_as_identifier(&tok.token)
            }
            other => {
                return Err(DbError::ParseError {
                    message: format!("expected identifier but found {:?}", other,),
                    position: Some(pos),
                })
            }
        };
        validate_identifier_length(&name, pos)?;
        Ok(name)
    }

    /// Parse `[schema '.'] name` as a [`TableRef`].
    pub(crate) fn parse_table_ref(&mut self) -> Result<TableRef, DbError> {
        let first = self.parse_identifier()?;
        if self.eat(&Token::Dot) {
            let second = self.parse_identifier()?;
            if self.eat(&Token::Dot) {
                // database.schema.table
                let third = self.parse_identifier()?;
                Ok(TableRef {
                    database: Some(first),
                    schema: Some(second),
                    name: third,
                    alias: None,
                })
            } else {
                // schema.table
                Ok(TableRef {
                    database: None,
                    schema: Some(first),
                    name: second,
                    alias: None,
                })
            }
        } else {
            // table
            Ok(TableRef {
                database: None,
                schema: None,
                name: first,
                alias: None,
            })
        }
    }

    // ── Top-level dispatch ─────────────────────────────────────────────────────

    pub(crate) fn parse_stmt(&mut self) -> Result<Stmt, DbError> {
        match self.peek() {
            Token::Create => {
                self.advance();
                self.parse_create()
            }
            Token::Drop => {
                self.advance();
                self.parse_drop()
            }
            Token::Select
            | Token::Insert
            | Token::Merge
            | Token::Update
            | Token::Delete
            | Token::With => {
                dml::parse_dml(self)
            }
            // MySQL `REPLACE INTO` — distinct statement verb, not a function
            // here (REPLACE() as a function still works in expression context
            // via parse_ident_or_call because the `(` follows immediately).
            // Also accepted at the dispatcher level: `REPLACE LOW_PRIORITY INTO`,
            // `REPLACE DELAYED INTO`, and `REPLACE IGNORE INTO` (the last is
            // invalid MySQL — parse_replace rejects it with a clear error).
            Token::Ident(s) | Token::QuotedIdent(s)
                if s.eq_ignore_ascii_case("replace")
                    && {
                        matches!(self.peek_at(1), Token::Into | Token::Ignore)
                            || matches!(self.peek_at(1), Token::Ident(k)
                                if k.eq_ignore_ascii_case("low_priority")
                                    || k.eq_ignore_ascii_case("delayed")
                                    || k.eq_ignore_ascii_case("high_priority"))
                    } =>
            {
                self.advance();
                dml::parse_replace(self)
            }
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("declare") => {
                self.advance();
                self.parse_declare_cursor()
            }
            Token::Fetch => {
                self.advance();
                self.parse_fetch_cursor()
            }
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("close") => {
                self.advance();
                self.parse_close_cursor()
            }
            Token::Truncate => {
                self.advance();
                // TRUNCATE [TABLE] table_name
                self.eat(&Token::Table);
                let table = self.parse_table_ref()?;
                // Consume optional alias (some clients send it)
                Ok(Stmt::TruncateTable(crate::ast::TruncateTableStmt { table }))
            }
            Token::SavepointKw => {
                self.advance();
                let name = self.parse_identifier()?;
                Ok(Stmt::Savepoint(name))
            }
            Token::Release => {
                self.advance();
                // RELEASE [SAVEPOINT] name
                self.eat(&Token::SavepointKw); // optional SAVEPOINT keyword
                let name = self.parse_identifier()?;
                Ok(Stmt::ReleaseSavepoint(name))
            }
            Token::Explain => {
                self.advance();
                // EXPLAIN <any statement> — wraps the inner statement.
                let inner = self.parse_stmt()?;
                Ok(Stmt::Explain(Box::new(inner)))
            }
            Token::Vacuum => {
                self.advance();
                // VACUUM [table_name]
                let table = if matches!(self.peek(), Token::Eof | Token::Semicolon) {
                    None
                } else {
                    Some(self.parse_table_ref()?)
                };
                Ok(Stmt::Vacuum(crate::ast::VacuumStmt { table }))
            }
            Token::Checkpoint => {
                self.advance();
                Ok(Stmt::Checkpoint)
            }
            Token::Refresh => {
                self.advance();
                ddl::parse_refresh_materialized_view(self)
            }
            Token::Analyze => {
                self.advance();
                // ANALYZE [TABLE name [(column)]]
                let table = if self.eat(&Token::Table) {
                    Some(self.parse_identifier()?)
                } else {
                    None
                };
                let column = if self.peek() == &Token::LParen {
                    self.advance();
                    let col = self.parse_identifier()?;
                    self.expect(&Token::RParen)?;
                    Some(col)
                } else {
                    None
                };
                Ok(Stmt::Analyze(crate::ast::AnalyzeStmt { table, column }))
            }
            Token::Show => {
                self.advance();
                // Optional FULL modifier for SHOW FULL TABLES / SHOW FULL COLUMNS (5.9f).
                // `FULL` is a reserved token (Token::Full), not an identifier.
                let full = self.eat(&Token::Full);
                // Optional GLOBAL / SESSION modifier for SHOW VARIABLES (already
                // handled by the interceptor, but consume it so we don't error).
                let _global = self.eat_ident_ci("GLOBAL") || self.eat_ident_ci("SESSION");
                match self.peek().clone() {
                    Token::Databases => {
                        self.advance();
                        Ok(Stmt::ShowDatabases(crate::ast::ShowDatabasesStmt))
                    }
                    Token::Tables => {
                        self.advance();
                        let schema = if self.eat(&Token::From) || self.eat_ident_ci("IN") {
                            Some(self.parse_identifier()?)
                        } else {
                            None
                        };
                        // Consume optional LIKE 'pattern' (ignored by executor — full list returned).
                        if self.eat(&Token::Like) {
                            let _ = self.parse_string_literal();
                        }
                        Ok(Stmt::ShowTables(crate::ast::ShowTablesStmt { schema, full }))
                    }
                    // SHOW TABLE STATUS [FROM db] [LIKE 'pattern']
                    Token::Table => {
                        self.advance();
                        if self.eat_ident_ci("STATUS") {
                            let schema = if self.eat(&Token::From) || self.eat_ident_ci("IN") {
                                Some(self.parse_identifier()?)
                            } else {
                                None
                            };
                            let like_pattern = if self.eat(&Token::Like) {
                                Some(self.parse_string_literal()?)
                            } else {
                                None
                            };
                            return Ok(Stmt::ShowTableStatus(
                                crate::ast::ShowTableStatusStmt { schema, like_pattern },
                            ));
                        }
                        // SHOW CREATE TABLE handled below via Token::Create,
                        // but SHOW TABLE STATUS consumed here so rewind is not possible.
                        // If we're here and it's not STATUS, it's a parse error.
                        Err(DbError::ParseError {
                            message: "expected STATUS after SHOW TABLE".into(),
                            position: Some(self.current_pos()),
                        })
                    }
                    // SHOW CREATE TABLE t
                    Token::Create => {
                        self.advance();
                        match self.peek() {
                            Token::Table => {
                                self.advance();
                                let table = self.parse_table_ref()?;
                                Ok(Stmt::ShowCreateTable(crate::ast::ShowCreateTableStmt { table }))
                            }
                            Token::Trigger => {
                                self.advance();
                                let name = self.parse_identifier()?;
                                self.expect(&Token::On)?;
                                let table = self.parse_table_ref()?;
                                Ok(Stmt::ShowCreateTrigger(
                                    crate::ast::ShowCreateTriggerStmt { name, table },
                                ))
                            }
                            other => Err(DbError::ParseError {
                                message: format!(
                                    "expected TABLE or TRIGGER after SHOW CREATE, found {other:?}"
                                ),
                                position: Some(self.current_pos()),
                            }),
                        }
                    }
                    // COLUMNS / FIELDS are not reserved keywords — they tokenize as Ident.
                    Token::Ident(kw) | Token::QuotedIdent(kw)
                        if kw.eq_ignore_ascii_case("columns")
                            || kw.eq_ignore_ascii_case("fields") =>
                    {
                        self.advance();
                        self.expect(&Token::From)?;
                        let table = self.parse_table_ref()?;
                        // Consume optional LIKE / WHERE filter (ignored — full list returned).
                        if self.eat(&Token::Like) {
                            let _ = self.parse_string_literal();
                        }
                        Ok(Stmt::ShowColumns(crate::ast::ShowColumnsStmt { table, full }))
                    }
                    // SHOW INDEX FROM table
                    Token::Index => {
                        self.advance();
                        self.expect(&Token::From)?;
                        let table = self.parse_table_ref()?;
                        Ok(Stmt::ShowIndex(crate::ast::ShowIndexStmt { table }))
                    }
                    // SHOW INDEXES / SHOW KEYS FROM table
                    Token::Ident(kw)
                        if kw.eq_ignore_ascii_case("indexes")
                            || kw.eq_ignore_ascii_case("keys") =>
                    {
                        self.advance();
                        self.expect(&Token::From)?;
                        let table = self.parse_table_ref()?;
                        Ok(Stmt::ShowIndex(crate::ast::ShowIndexStmt { table }))
                    }
                    // SHOW WARNINGS [LIMIT n] / SHOW ERRORS [LIMIT n] — 5.9e
                    Token::Ident(kw) if kw.eq_ignore_ascii_case("warnings") => {
                        self.advance();
                        let limit = if self.eat(&Token::Limit) {
                            match self.peek().clone() {
                                Token::Integer(n) => { self.advance(); Some(n as u64) }
                                _ => None,
                            }
                        } else {
                            None
                        };
                        Ok(Stmt::ShowWarnings { limit })
                    }
                    Token::Ident(kw) if kw.eq_ignore_ascii_case("notifications") => {
                        self.advance();
                        Ok(Stmt::ShowNotifications)
                    }
                    Token::Ident(kw) if kw.eq_ignore_ascii_case("errors") => {
                        self.advance();
                        let limit = if self.eat(&Token::Limit) {
                            match self.peek().clone() {
                                Token::Integer(n) => { self.advance(); Some(n as u64) }
                                _ => None,
                            }
                        } else {
                            None
                        };
                        Ok(Stmt::ShowErrors { limit })
                    }
                    // SHOW ENGINES — DB tools (5.9g)
                    Token::Ident(kw) if kw.eq_ignore_ascii_case("engines") => {
                        self.advance();
                        Ok(Stmt::ShowEngines)
                    }
                    // SHOW CHARSET / SHOW CHARACTER SET — DB tools (5.9g)
                    Token::Ident(kw)
                        if kw.eq_ignore_ascii_case("charset")
                            || kw.eq_ignore_ascii_case("character") =>
                    {
                        self.advance();
                        self.eat_ident_ci("SET");
                        // Optional LIKE / WHERE — consume and discard
                        if self.eat(&Token::Like) {
                            let _ = self.parse_string_literal();
                        }
                        Ok(Stmt::ShowCharset)
                    }
                    // SHOW COLLATION — DB tools (5.9g)
                    Token::Ident(kw) if kw.eq_ignore_ascii_case("collation") => {
                        self.advance();
                        if self.eat(&Token::Like) {
                            let _ = self.parse_string_literal();
                        }
                        Ok(Stmt::ShowCollation)
                    }
                    // SHOW VARIABLES is intercepted by the wire handler, but we need
                    // a fallback in case the executor is called directly.
                    Token::Ident(kw) if kw.eq_ignore_ascii_case("variables") => {
                        self.advance();
                        if self.eat(&Token::Like) {
                            let _ = self.parse_string_literal();
                        }
                        Ok(Stmt::ShowVariables)
                    }
                    // SHOW STATUS handled like SHOW VARIABLES
                    Token::Ident(kw) if kw.eq_ignore_ascii_case("status") => {
                        self.advance();
                        if self.eat(&Token::Like) {
                            let _ = self.parse_string_literal();
                        }
                        Ok(Stmt::ShowStatus)
                    }
                    other => Err(DbError::ParseError {
                        message: format!(
                            "unexpected token after SHOW: {other:?}"
                        ),
                        position: Some(self.current_pos()),
                    }),
                }
            }
            // RENAME TABLE old TO new [, old2 TO new2 ...]
            Token::Rename => {
                self.advance();
                self.expect(&Token::Table)?;
                let mut pairs = Vec::new();
                loop {
                    let old_name = self.parse_identifier()?;
                    self.expect(&Token::To)?;
                    let new_name = self.parse_identifier()?;
                    pairs.push((old_name, new_name));
                    if !self.eat(&Token::Comma) {
                        break;
                    }
                }
                Ok(Stmt::RenameTable(crate::ast::RenameTableStmt { pairs }))
            }
            // DESCRIBE table_name / DESC table_name
            Token::Describe => {
                self.advance();
                let table = self.parse_table_ref()?;
                Ok(Stmt::ShowColumns(crate::ast::ShowColumnsStmt { table, full: false }))
            }
            Token::Alter => {
                self.advance();
                self.expect(&Token::Table)?;
                ddl::parse_alter_table(self)
            }
            Token::Begin => {
                self.advance();
                // Accept optional WORK or TRANSACTION keyword (MySQL: BEGIN WORK)
                self.eat(&Token::Work);
                self.eat(&Token::Transaction);
                Ok(Stmt::Begin)
            }
            Token::Start => {
                self.advance();
                self.eat(&Token::Transaction);
                // MySQL `START TRANSACTION [option_list]` — option_list is a
                // comma-separated sequence of:
                //   READ ONLY | READ WRITE | WITH CONSISTENT SNAPSHOT
                // All three are accepted but effectively no-ops: AxiomDB's
                // optimistic MVCC already gives a consistent snapshot per txn
                // and read-only / read-write are superficial until Phase 13.7.
                loop {
                    if self.eat(&Token::Read) {
                        self.eat(&Token::Only);
                        self.eat(&Token::Write);
                    } else if self.eat(&Token::With) {
                        // WITH CONSISTENT SNAPSHOT — `CONSISTENT` + `SNAPSHOT`
                        // arrive as identifiers.
                        if let Token::Ident(s) = self.peek().clone() {
                            if s.eq_ignore_ascii_case("consistent") {
                                self.advance();
                                if let Token::Ident(s2) = self.peek().clone() {
                                    if s2.eq_ignore_ascii_case("snapshot") {
                                        self.advance();
                                    }
                                }
                            }
                        }
                    } else {
                        break;
                    }
                    if !self.eat(&Token::Comma) {
                        break;
                    }
                }
                Ok(Stmt::Begin)
            }
            Token::Commit => {
                self.advance();
                Ok(Stmt::Commit)
            }
            Token::Listen => {
                self.advance();
                let channel = self.parse_identifier()?;
                Ok(Stmt::Listen(crate::ast::ListenStmt { channel }))
            }
            Token::Unlisten => {
                self.advance();
                let channel = if self.eat(&Token::Star) {
                    None
                } else {
                    Some(self.parse_identifier()?)
                };
                Ok(Stmt::Unlisten(crate::ast::UnlistenStmt { channel }))
            }
            Token::Notify => {
                self.advance();
                let channel = self.parse_identifier()?;
                let payload = if self.eat(&Token::Comma) {
                    Some(expr::parse_expr(self)?)
                } else {
                    None
                };
                Ok(Stmt::Notify(crate::ast::NotifyStmt { channel, payload }))
            }
            Token::Rollback => {
                self.advance();
                // ROLLBACK TO [SAVEPOINT] name — rollback to named savepoint
                if self.eat(&Token::To) {
                    self.eat(&Token::SavepointKw); // optional SAVEPOINT keyword
                    let name = self.parse_identifier()?;
                    Ok(Stmt::RollbackToSavepoint(name))
                } else {
                    Ok(Stmt::Rollback)
                }
            }
            Token::Set => {
                self.advance(); // consume SET
                // Skip optional SESSION / GLOBAL / LOCAL scope prefix.
                // These are MySQL syntax (SET SESSION var = val, SET GLOBAL var = val).
                // AxiomDB applies all settings at session scope regardless.
                self.eat(&Token::Session);
                self.eat(&Token::Global);
                self.eat(&Token::Local);
                // `SET [SESSION|GLOBAL] TRANSACTION ISOLATION LEVEL <level>` —
                // SQL-standard syntax. Also `SET TRANSACTION READ {ONLY|WRITE}`.
                // Both map to the existing session variables.
                if self.eat(&Token::Transaction) {
                    if let Some(stmt) = self.parse_set_transaction_tail()? {
                        return Ok(stmt);
                    }
                }
                // Skip optional @@ or @ prefix (already consumed as part of identifier
                // in the wire-level interceptor; here we handle the raw SQL path).
                let variable = self.parse_set_variable()?;
                self.expect(&Token::Eq)?;
                let value = self.parse_set_value()?;
                // Silently skip additional `, var = val` pairs — multi-var SET.
                // Each additional pair is parsed and thrown away; only the first
                // variable takes effect. This is sufficient for mysqldump compatibility
                // where `SET NAMES utf8mb4, collation_connection = utf8mb4_unicode_ci`
                // is common.
                while self.eat(&Token::Comma) {
                    self.eat(&Token::Session);
                    self.eat(&Token::Global);
                    self.eat(&Token::Local);
                    let _ = self.parse_set_variable();
                    if self.eat(&Token::Eq) {
                        let _ = self.parse_set_value();
                    }
                }
                Ok(Stmt::Set(SetStmt { variable, value }))
            }

            // ── MySQL no-op statements ────────────────────────────────────────────
            // These are common in mysqldump output or sent by MySQL clients.
            // Parse and discard them cleanly.
            Token::Lock => {
                self.advance(); // LOCK
                // Consume everything up to the end of the statement.
                self.skip_until_statement_end();
                Ok(Stmt::Noop)
            }
            Token::Unlock => {
                self.advance(); // UNLOCK
                self.skip_until_statement_end();
                Ok(Stmt::Noop)
            }
            Token::Flush => {
                self.advance(); // FLUSH
                self.skip_until_statement_end();
                Ok(Stmt::Noop)
            }
            Token::Kill => {
                self.advance(); // KILL
                self.skip_until_statement_end();
                Ok(Stmt::Noop)
            }
            Token::Use => {
                self.advance();
                let name = self.parse_identifier()?;
                Ok(Stmt::UseDatabase(crate::ast::UseDatabaseStmt { name }))
            }
            // ── MySQL CALL / DO ───────────────────────────────────────────────────
            Token::Call => {
                self.advance(); // consume CALL
                // Parse qualified or unqualified procedure name
                let mut name = self.parse_identifier()?;
                // Handle schema.proc form
                if self.eat(&Token::Dot) {
                    let proc = self.parse_identifier()?;
                    name = format!("{}.{}", name, proc);
                }
                // Parse argument list (may be empty)
                let mut args = vec![];
                if self.eat(&Token::LParen) {
                    if !matches!(self.peek(), Token::RParen) {
                        args.push(dml::parse_call_arg(self)?);
                        while self.eat(&Token::Comma) {
                            args.push(dml::parse_call_arg(self)?);
                        }
                    }
                    self.expect(&Token::RParen)?;
                }
                Ok(Stmt::Call { name, args })
            }
            Token::Do => {
                self.advance(); // consume DO
                let expr = dml::parse_do_expr(self)?;
                Ok(Stmt::Do { expr })
            }
            Token::Eof => Err(DbError::ParseError {
                message: "empty input: no SQL statement found".into(),
                position: Some(self.current_pos()),
            }),
            other => Err(DbError::ParseError {
                message: format!(
                    "unexpected token {:?} — expected SELECT, INSERT, UPDATE, DELETE, CREATE, DROP, BEGIN, COMMIT, ROLLBACK, LISTEN, UNLISTEN, or NOTIFY",
                    other,
                ),
                position: Some(self.current_pos()),
            }),
        }
    }

    fn parse_declare_cursor(&mut self) -> Result<Stmt, DbError> {
        let name = self.parse_identifier()?;
        if !self.eat_ident_ci("CURSOR") {
            return Err(DbError::ParseError {
                message: "expected CURSOR after DECLARE <name>".into(),
                position: Some(self.current_pos()),
            });
        }
        self.expect(&Token::For)?;
        let query = self.parse_stmt()?;
        match query {
            Stmt::Select(_) | Stmt::SetOp { .. } => {
                Ok(Stmt::DeclareCursor(crate::ast::DeclareCursorStmt {
                    name,
                    query: Box::new(query),
                }))
            }
            other => Err(DbError::ParseError {
                message: format!(
                    "DECLARE CURSOR requires a row-returning query, found {:?}",
                    other
                ),
                position: Some(self.current_pos()),
            }),
        }
    }

    fn parse_fetch_cursor(&mut self) -> Result<Stmt, DbError> {
        let count = match self.peek().clone() {
            Token::Next => {
                self.advance();
                crate::ast::FetchCount::Next
            }
            Token::All => {
                self.advance();
                crate::ast::FetchCount::All
            }
            Token::Integer(n) if n >= 0 => {
                self.advance();
                crate::ast::FetchCount::Forward(n as u64)
            }
            Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("forward") => {
                self.advance();
                match self.peek().clone() {
                    Token::Integer(n) if n >= 0 => {
                        self.advance();
                        crate::ast::FetchCount::Forward(n as u64)
                    }
                    other => {
                        return Err(DbError::ParseError {
                            message: format!(
                                "expected non-negative integer after FETCH FORWARD, found {other:?}"
                            ),
                            position: Some(self.current_pos()),
                        });
                    }
                }
            }
            other => {
                return Err(DbError::ParseError {
                    message: format!(
                        "expected NEXT, ALL, integer count, or FORWARD after FETCH, found {other:?}"
                    ),
                    position: Some(self.current_pos()),
                });
            }
        };

        if !(self.eat(&Token::From) || self.eat(&Token::In)) {
            return Err(DbError::ParseError {
                message: "expected FROM or IN after FETCH selector".into(),
                position: Some(self.current_pos()),
            });
        }
        let name = self.parse_identifier()?;
        Ok(Stmt::FetchCursor(crate::ast::FetchCursorStmt {
            name,
            count,
        }))
    }

    fn parse_close_cursor(&mut self) -> Result<Stmt, DbError> {
        if self.eat(&Token::All) {
            return Ok(Stmt::CloseCursor(crate::ast::CloseCursorStmt::All));
        }
        let name = self.parse_identifier()?;
        Ok(Stmt::CloseCursor(crate::ast::CloseCursorStmt::One(name)))
    }

    fn parse_create(&mut self) -> Result<Stmt, DbError> {
        match self.peek() {
            Token::Database => {
                self.advance();
                ddl::parse_create_database(self)
            }
            Token::Schema => {
                self.advance();
                ddl::parse_create_schema(self)
            }
            Token::Temp => {
                self.advance();
                self.expect(&Token::Table)?;
                ddl::parse_create_table(self, axiomdb_catalog::TablePersistence::Temporary)
            }
            Token::Unlogged => {
                self.advance();
                self.expect(&Token::Table)?;
                ddl::parse_create_table(self, axiomdb_catalog::TablePersistence::Unlogged)
            }
            Token::Table => {
                self.advance();
                ddl::parse_create_table(self, axiomdb_catalog::TablePersistence::Permanent)
            }
            Token::Materialized => {
                self.advance();
                self.expect(&Token::View)?;
                ddl::parse_create_materialized_view(self)
            }
            Token::Trigger => {
                self.advance();
                ddl::parse_create_trigger(self)
            }
            Token::Unique => {
                self.advance();
                self.expect(&Token::Index)?;
                ddl::parse_create_index(self, true)
            }
            Token::Index => {
                self.advance();
                ddl::parse_create_index(self, false)
            }
            other => Err(DbError::ParseError {
                message: format!(
                    "expected DATABASE, TEMP[TORARY] TABLE, UNLOGGED TABLE, TABLE, MATERIALIZED VIEW, TRIGGER or INDEX after CREATE, found {:?}",
                    other,
                ),
                position: Some(self.current_pos()),
            }),
        }
    }

    fn parse_drop(&mut self) -> Result<Stmt, DbError> {
        match self.peek() {
            Token::Database => {
                self.advance();
                ddl::parse_drop_database(self)
            }
            Token::Table => {
                self.advance();
                ddl::parse_drop_table(self)
            }
            Token::Materialized => {
                self.advance();
                self.expect(&Token::View)?;
                ddl::parse_drop_materialized_view(self)
            }
            Token::Index => {
                self.advance();
                ddl::parse_drop_index(self)
            }
            Token::Trigger => {
                self.advance();
                ddl::parse_drop_trigger(self)
            }
            other => Err(DbError::ParseError {
                message: format!(
                    "expected DATABASE, TABLE, MATERIALIZED VIEW, TRIGGER or INDEX after DROP, found {:?}",
                    other,
                ),
                position: Some(self.current_pos()),
            }),
        }
    }

    /// Parses the value side of a `SET variable = <value>` statement.
    ///
    /// Handles the common MySQL SET value forms:
    /// - `DEFAULT`    → `SetValue::Default`
    /// - `ON`         → `SetValue::Expr(Literal(Text("ON")))`
    /// - `OFF`        → `SetValue::Expr(Literal(Text("OFF")))`
    /// - `TRUE`       → `SetValue::Expr(Literal(Bool(true)))`
    /// - `FALSE`      → `SetValue::Expr(Literal(Bool(false)))`
    /// - Any other expression (literals, integers, strings) → `SetValue::Expr`
    ///
    /// Parse a SET variable name, stripping any `@@` / `@` prefix and optional
    /// scope qualifier such as `session.` or `global.`.
    pub(crate) fn parse_set_variable(&mut self) -> Result<String, DbError> {
        if self.eat(&Token::AtAt) || self.eat(&Token::At) {
            // Optional scope prefix after @@, e.g. @@session.autocommit.
            if matches!(self.peek(), Token::Session | Token::Global | Token::Local) {
                let _ = self.parse_identifier()?;
                let _ = self.eat(&Token::Dot);
            }

            let mut name = self.parse_identifier()?;
            while self.eat(&Token::Dot) {
                name = self.parse_identifier()?;
            }
            return Ok(name);
        }

        // Backward-compatible fallback in case the prefix arrives as part of a
        // single identifier token in some caller or future lexer variant.
        let name = self.parse_identifier()?;
        let stripped = name.trim_start_matches('@');
        Ok(stripped.rsplit('.').next().unwrap_or(stripped).to_string())
    }

    /// Consume tokens until EOF or semicolon (end of statement), used for
    /// no-op statements (LOCK, UNLOCK, FLUSH, KILL) where we want to parse
    /// cleanly without interpreting the arguments.
    pub(crate) fn skip_until_statement_end(&mut self) {
        loop {
            match self.peek() {
                Token::Eof | Token::Semicolon => break,
                _ => {
                    self.pos += 1;
                }
            }
        }
    }

    /// Parses the tail of `SET [SESSION|GLOBAL] TRANSACTION ...`.
    ///
    /// Supports:
    /// - `ISOLATION LEVEL READ UNCOMMITTED | READ COMMITTED | REPEATABLE READ | SERIALIZABLE`
    ///   → `SET transaction_isolation = '<level>'`
    /// - `READ ONLY` / `READ WRITE`
    ///   → `SET transaction_read_only = ON|OFF`
    ///
    /// Returns `Ok(None)` when the tokens don't match any known form — the
    /// caller then falls through to the generic `var = value` path so other
    /// vendor dialects aren't accidentally rejected.
    pub(crate) fn parse_set_transaction_tail(&mut self) -> Result<Option<Stmt>, DbError> {
        // ISOLATION LEVEL <level>
        if let Token::Ident(s) = self.peek().clone() {
            if s.eq_ignore_ascii_case("isolation") {
                self.advance();
                if let Token::Ident(lvl) = self.peek().clone() {
                    if !lvl.eq_ignore_ascii_case("level") {
                        return Err(DbError::ParseError {
                            message: format!("expected LEVEL after ISOLATION, found {lvl}"),
                            position: Some(self.current_pos()),
                        });
                    }
                    self.advance(); // LEVEL
                }
                // Level identifier: READ UNCOMMITTED | READ COMMITTED |
                // REPEATABLE READ | SERIALIZABLE.
                let level = self.parse_isolation_level_words()?;
                return Ok(Some(Stmt::Set(SetStmt {
                    variable: "transaction_isolation".into(),
                    value: SetValue::Expr(crate::expr::Expr::Literal(axiomdb_types::Value::Text(
                        level,
                    ))),
                })));
            }
        }
        // READ ONLY / READ WRITE
        if self.eat(&Token::Read) {
            if self.eat(&Token::Only) {
                return Ok(Some(Stmt::Set(SetStmt {
                    variable: "transaction_read_only".into(),
                    value: SetValue::Expr(crate::expr::Expr::Literal(axiomdb_types::Value::Text(
                        "ON".into(),
                    ))),
                })));
            }
            if self.eat(&Token::Write) {
                return Ok(Some(Stmt::Set(SetStmt {
                    variable: "transaction_read_only".into(),
                    value: SetValue::Expr(crate::expr::Expr::Literal(axiomdb_types::Value::Text(
                        "OFF".into(),
                    ))),
                })));
            }
        }
        Ok(None)
    }

    fn parse_isolation_level_words(&mut self) -> Result<String, DbError> {
        // First word: READ (then UNCOMMITTED|COMMITTED), REPEATABLE (then READ),
        // or SERIALIZABLE.
        let first = match self.peek().clone() {
            Token::Read => {
                self.advance();
                "read"
            }
            Token::Ident(s) => {
                self.advance();
                return match s.to_ascii_lowercase().as_str() {
                    "repeatable" => {
                        // Expect READ.
                        match self.peek().clone() {
                            Token::Read => {
                                self.advance();
                                Ok("repeatable read".into())
                            }
                            other => Err(DbError::ParseError {
                                message: format!("expected READ after REPEATABLE, found {other:?}"),
                                position: Some(self.current_pos()),
                            }),
                        }
                    }
                    "serializable" => Ok("serializable".into()),
                    other => Err(DbError::ParseError {
                        message: format!("unknown isolation level '{other}'"),
                        position: Some(self.current_pos()),
                    }),
                };
            }
            other => {
                return Err(DbError::ParseError {
                    message: format!("expected isolation level, found {other:?}"),
                    position: Some(self.current_pos()),
                })
            }
        };
        // After READ: UNCOMMITTED or COMMITTED.
        match self.peek().clone() {
            Token::Ident(s) => {
                let low = s.to_ascii_lowercase();
                if low == "uncommitted" || low == "committed" {
                    self.advance();
                    Ok(format!("{first} {low}"))
                } else {
                    Err(DbError::ParseError {
                        message: format!("expected UNCOMMITTED or COMMITTED after READ, found {s}"),
                        position: Some(self.current_pos()),
                    })
                }
            }
            other => Err(DbError::ParseError {
                message: format!("expected UNCOMMITTED or COMMITTED after READ, found {other:?}"),
                position: Some(self.current_pos()),
            }),
        }
    }

    pub(crate) fn parse_set_value(&mut self) -> Result<SetValue, DbError> {
        use crate::expr::Expr;
        match self.peek().clone() {
            Token::Default => {
                self.advance();
                Ok(SetValue::Default)
            }
            Token::On => {
                self.advance();
                Ok(SetValue::Expr(Expr::Literal(Value::Text("ON".into()))))
            }
            Token::Ident(s) if s.eq_ignore_ascii_case("off") => {
                self.advance();
                Ok(SetValue::Expr(Expr::Literal(Value::Text("OFF".into()))))
            }
            Token::True => {
                self.advance();
                Ok(SetValue::Expr(Expr::Literal(Value::Bool(true))))
            }
            Token::False => {
                self.advance();
                Ok(SetValue::Expr(Expr::Literal(Value::Bool(false))))
            }
            _ => Ok(SetValue::Expr(expr::parse_expr(self)?)),
        }
    }
}

// ── Identifier helpers ────────────────────────────────────────────────────────

const MAX_IDENTIFIER_LEN: usize = 64;

fn validate_identifier_length(name: &str, pos: usize) -> Result<(), DbError> {
    if name.len() > MAX_IDENTIFIER_LEN {
        return Err(DbError::ParseError {
            message: format!(
                "identifier '{}' exceeds maximum length of {} characters ({} chars)",
                name,
                MAX_IDENTIFIER_LEN,
                name.len(),
            ),
            position: Some(pos),
        });
    }
    Ok(())
}

/// Convert a keyword token to its string representation (for unreserved keyword
/// use as identifier).
fn keyword_as_identifier(tok: &Token<'_>) -> String {
    match tok {
        Token::Key => "key".into(),
        Token::Index => "index".into(),
        Token::Tables => "tables".into(),
        Token::Desc => "desc".into(),
        Token::Set => "set".into(),
        Token::Action => "action".into(),
        Token::Names => "names".into(),
        Token::Autocommit => "autocommit".into(),
        // MySQL compatibility keywords usable as identifiers
        Token::Work => "work".into(),
        Token::Read => "read".into(),
        Token::Only => "only".into(),
        Token::Write => "write".into(),
        Token::Global => "global".into(),
        Token::Session => "session".into(),
        Token::Local => "local".into(),
        Token::Lock => "lock".into(),
        Token::Unlock => "unlock".into(),
        Token::Flush => "flush".into(),
        Token::Checkpoint => "checkpoint".into(),
        Token::Kill => "kill".into(),
        Token::Query => "query".into(),
        Token::Connection => "connection".into(),
        Token::For => "for".into(),
        // Expression operator keywords usable as identifiers
        Token::Regexp => "regexp".into(),
        Token::Rlike => "rlike".into(),
        Token::Xor => "xor".into(),
        Token::IntDiv => "div".into(),
        // DML keywords that double as MySQL built-in function names.
        Token::Truncate => "truncate".into(),
        Token::Insert => "insert".into(),
        Token::Merge => "merge".into(),
        _ => unreachable!("only called for known unreserved keywords"),
    }
}
