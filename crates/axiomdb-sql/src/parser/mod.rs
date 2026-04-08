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
    let mut p = Parser::new(&tokens);
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
    let mut p = Parser::new(&tokens);
    let e = expr::parse_expr(&mut p)?;
    Ok(e)
}

// ── Parser struct ─────────────────────────────────────────────────────────────

/// Recursive descent parser over a slice of [`SpannedToken`]s.
///
/// The lifetime `'src` is tied to the original SQL input string.
pub(crate) struct Parser<'src> {
    tokens: &'src [SpannedToken<'src>],
    pos: usize,
    /// Parameter index counter for `?` placeholders in prepared statement templates.
    /// Incremented each time `Token::Question` is consumed via `parse_atom`.
    pub(crate) param_count: usize,
}

impl<'src> Parser<'src> {
    pub(crate) fn new(tokens: &'src [SpannedToken<'src>]) -> Self {
        Self {
            tokens,
            pos: 0,
            param_count: 0,
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
            Token::Select | Token::Insert | Token::Update | Token::Delete => {
                dml::parse_dml(self)
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
                        self.expect(&Token::Table)?;
                        let table = self.parse_table_ref()?;
                        Ok(Stmt::ShowCreateTable(crate::ast::ShowCreateTableStmt { table }))
                    }
                    // COLUMNS is not a reserved keyword — it tokenizes as Ident
                    Token::Ident(kw) | Token::QuotedIdent(kw) if kw.eq_ignore_ascii_case("columns") => {
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
                // Optional READ ONLY / READ WRITE modifier — consumed and ignored
                // (AxiomDB uses optimistic MVCC; both modes work the same until Phase 13.7)
                if self.eat(&Token::Read) {
                    self.eat(&Token::Only);
                    self.eat(&Token::Write);
                }
                Ok(Stmt::Begin)
            }
            Token::Commit => {
                self.advance();
                Ok(Stmt::Commit)
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
                    "unexpected token {:?} — expected SELECT, INSERT, UPDATE, DELETE, CREATE, DROP, BEGIN, COMMIT, or ROLLBACK",
                    other,
                ),
                position: Some(self.current_pos()),
            }),
        }
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
            Token::Table => {
                self.advance();
                ddl::parse_create_table(self)
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
                    "expected DATABASE, TABLE or INDEX after CREATE, found {:?}",
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
            Token::Index => {
                self.advance();
                ddl::parse_drop_index(self)
            }
            other => Err(DbError::ParseError {
                message: format!(
                    "expected DATABASE, TABLE or INDEX after DROP, found {:?}",
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
    /// Parse a SET variable name, stripping any `@@` or `@` prefix.
    ///
    /// MySQL sends `SET @@session.autocommit = 1` and `SET @user_var = 1`.
    /// We strip the `@` characters and split on `.` to get the bare name.
    pub(crate) fn parse_set_variable(&mut self) -> Result<String, DbError> {
        // Handle `@@variable` — two `@` signs followed by an identifier
        // The lexer tokenizes `@@autocommit` as `Ident("@@autocommit")` (no explicit @@ token).
        // But if it's tokenized differently, fall back to parse_identifier.
        let name = self.parse_identifier()?;
        // Strip leading `@` chars (e.g. "@@session.autocommit" → "autocommit")
        let stripped = name.trim_start_matches('@');
        // Strip optional scope prefix (e.g. "session.autocommit" → "autocommit")
        let bare = if let Some(dot_pos) = stripped.rfind('.') {
            &stripped[dot_pos + 1..]
        } else {
            stripped
        };
        Ok(bare.to_string())
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
        Token::Kill => "kill".into(),
        Token::Query => "query".into(),
        Token::Connection => "connection".into(),
        // Expression operator keywords usable as identifiers
        Token::Regexp => "regexp".into(),
        Token::Rlike => "rlike".into(),
        Token::Xor => "xor".into(),
        Token::IntDiv => "div".into(),
        // DML keywords that double as MySQL built-in function names.
        Token::Truncate => "truncate".into(),
        Token::Insert => "insert".into(),
        _ => unreachable!("only called for known unreserved keywords"),
    }
}
