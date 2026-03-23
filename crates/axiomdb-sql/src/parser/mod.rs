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

use crate::{
    ast::{Stmt, TableRef},
    lexer::{tokenize, Span, SpannedToken, Token},
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
    let tokens = tokenize(input, max_bytes)?;
    let mut p = Parser::new(&tokens);
    let stmt = p.parse_stmt()?;

    // After parsing, only Eof (or Semicolon+Eof) should remain.
    // For now, require exactly Eof (single-statement mode).
    // Multi-statement support comes in Phase 4.5.
    p.eat(&Token::Semicolon);
    if p.peek() != &Token::Eof {
        return Err(DbError::ParseError {
            message: format!(
                "unexpected token {:?} after statement at position {}",
                p.peek(),
                p.current_pos()
            ),
        });
    }

    Ok(stmt)
}

// ── Parser struct ─────────────────────────────────────────────────────────────

/// Recursive descent parser over a slice of [`SpannedToken`]s.
///
/// The lifetime `'src` is tied to the original SQL input string.
pub(crate) struct Parser<'src> {
    tokens: &'src [SpannedToken<'src>],
    pos: usize,
}

impl<'src> Parser<'src> {
    pub(crate) fn new(tokens: &'src [SpannedToken<'src>]) -> Self {
        Self { tokens, pos: 0 }
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
                message: format!(
                    "expected {:?} but found {:?} at position {}",
                    expected,
                    self.peek(),
                    self.current_pos()
                ),
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

    // ── Identifier helpers ─────────────────────────────────────────────────────

    /// Parse an unquoted or quoted identifier.
    ///
    /// Converts the zero-copy `&'src str` to an owned `String` exactly once.
    /// Validates the 64-character limit (4.3d).
    pub(crate) fn parse_identifier(&mut self) -> Result<String, DbError> {
        let pos = self.current_pos();
        let name = match self.peek().clone() {
            Token::Ident(s) | Token::QuotedIdent(s) | Token::DqIdent(s) => {
                self.pos += 1;
                s.to_string() // &'src str → String: the one allocation per identifier
            }
            // Allow certain keywords to be used as identifiers (unreserved words).
            Token::Key
            | Token::Index
            | Token::Tables
            | Token::Desc
            | Token::Set
            | Token::Action
            | Token::Names
            | Token::Autocommit => {
                let tok = self.advance();
                keyword_as_identifier(&tok.token)
            }
            other => {
                return Err(DbError::ParseError {
                    message: format!(
                        "expected identifier but found {:?} at position {}",
                        other, pos
                    ),
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
            let table = self.parse_identifier()?;
            Ok(TableRef {
                schema: Some(first),
                name: table,
                alias: None,
            })
        } else {
            Ok(TableRef {
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
            Token::Show => {
                self.advance();
                match self.peek().clone() {
                    Token::Tables => {
                        self.advance();
                        let schema = if self.eat(&Token::From) {
                            Some(self.parse_identifier()?)
                        } else {
                            None
                        };
                        Ok(Stmt::ShowTables(crate::ast::ShowTablesStmt { schema }))
                    }
                    // COLUMNS is not a reserved keyword — it tokenizes as Ident
                    Token::Ident(kw) | Token::QuotedIdent(kw) if kw.eq_ignore_ascii_case("columns") => {
                        self.advance();
                        self.expect(&Token::From)?;
                        let table = self.parse_table_ref()?;
                        Ok(Stmt::ShowColumns(crate::ast::ShowColumnsStmt { table }))
                    }
                    other => Err(DbError::ParseError {
                        message: format!(
                            "expected TABLES or COLUMNS after SHOW, found {:?} at position {}",
                            other, self.current_pos()
                        ),
                    }),
                }
            }
            // DESCRIBE table_name / DESC table_name
            Token::Describe => {
                self.advance();
                let table = self.parse_table_ref()?;
                Ok(Stmt::ShowColumns(crate::ast::ShowColumnsStmt { table }))
            }
            Token::Begin => {
                self.advance();
                // Accept optional TRANSACTION keyword
                self.eat(&Token::Transaction);
                Ok(Stmt::Begin)
            }
            Token::Start => {
                self.advance();
                self.eat(&Token::Transaction);
                Ok(Stmt::Begin)
            }
            Token::Commit => {
                self.advance();
                Ok(Stmt::Commit)
            }
            Token::Rollback => {
                self.advance();
                Ok(Stmt::Rollback)
            }
            Token::Eof => Err(DbError::ParseError {
                message: "empty input: no SQL statement found".into(),
            }),
            other => Err(DbError::ParseError {
                message: format!(
                    "unexpected token {:?} at position {} — expected SELECT, INSERT, UPDATE, DELETE, CREATE, DROP, BEGIN, COMMIT, or ROLLBACK",
                    other,
                    self.current_pos()
                ),
            }),
        }
    }

    fn parse_create(&mut self) -> Result<Stmt, DbError> {
        match self.peek() {
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
                    "expected TABLE or INDEX after CREATE, found {:?} at position {}",
                    other,
                    self.current_pos()
                ),
            }),
        }
    }

    fn parse_drop(&mut self) -> Result<Stmt, DbError> {
        match self.peek() {
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
                    "expected TABLE or INDEX after DROP, found {:?} at position {}",
                    other,
                    self.current_pos()
                ),
            }),
        }
    }
}

// ── Identifier helpers ────────────────────────────────────────────────────────

const MAX_IDENTIFIER_LEN: usize = 64;

fn validate_identifier_length(name: &str, pos: usize) -> Result<(), DbError> {
    if name.len() > MAX_IDENTIFIER_LEN {
        return Err(DbError::ParseError {
            message: format!(
                "identifier '{}' exceeds maximum length of {} characters ({} chars) at position {}",
                name,
                MAX_IDENTIFIER_LEN,
                name.len(),
                pos
            ),
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
        _ => unreachable!("only called for known unreserved keywords"),
    }
}
