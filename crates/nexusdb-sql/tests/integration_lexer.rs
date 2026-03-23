//! Integration tests for the SQL lexer (subfase 4.2 + 4.2b).

use nexusdb_core::DbError;
use nexusdb_sql::{tokenize, Token};

// ── Helpers ───────────────────────────────────────────────────────────────────

fn tokens(input: &str) -> Vec<Token> {
    tokenize(input, None)
        .unwrap()
        .into_iter()
        .map(|st| st.token)
        .collect()
}

fn tokens_err(input: &str) -> DbError {
    tokenize(input, None).unwrap_err()
}

// ── DML keywords ─────────────────────────────────────────────────────────────

#[test]
fn test_dml_keywords() {
    assert_eq!(
        tokens("SELECT FROM WHERE INSERT INTO VALUES UPDATE SET DELETE"),
        vec![
            Token::Select,
            Token::From,
            Token::Where,
            Token::Insert,
            Token::Into,
            Token::Values,
            Token::Update,
            Token::Set,
            Token::Delete,
            Token::Eof,
        ]
    );
}

// ── DDL keywords ─────────────────────────────────────────────────────────────

#[test]
fn test_ddl_keywords() {
    assert_eq!(
        tokens("CREATE TABLE INDEX DROP ALTER ADD COLUMN IF EXISTS TRUNCATE"),
        vec![
            Token::Create,
            Token::Table,
            Token::Index,
            Token::Drop,
            Token::Alter,
            Token::Add,
            Token::Column,
            Token::If,
            Token::Exists,
            Token::Truncate,
            Token::Eof,
        ]
    );
}

// ── Keyword case-insensitivity ────────────────────────────────────────────────

#[test]
fn test_keywords_case_insensitive() {
    assert_eq!(tokens("select"), vec![Token::Select, Token::Eof]);
    assert_eq!(tokens("SELECT"), vec![Token::Select, Token::Eof]);
    assert_eq!(tokens("Select"), vec![Token::Select, Token::Eof]);
    assert_eq!(tokens("sElEcT"), vec![Token::Select, Token::Eof]);
    assert_eq!(tokens("from"), vec![Token::From, Token::Eof]);
    assert_eq!(tokens("WHERE"), vec![Token::Where, Token::Eof]);
    assert_eq!(tokens("null"), vec![Token::Null, Token::Eof]);
    assert_eq!(tokens("TRUE"), vec![Token::True, Token::Eof]);
    assert_eq!(tokens("false"), vec![Token::False, Token::Eof]);
}

// ── Boolean / null literals ───────────────────────────────────────────────────

#[test]
fn test_boolean_and_null_keywords() {
    assert_eq!(tokens("TRUE"), vec![Token::True, Token::Eof]);
    assert_eq!(tokens("FALSE"), vec![Token::False, Token::Eof]);
    assert_eq!(tokens("NULL"), vec![Token::Null, Token::Eof]);
    assert_eq!(
        tokens("true false null"),
        vec![Token::True, Token::False, Token::Null, Token::Eof]
    );
}

// ── Transaction keywords ──────────────────────────────────────────────────────

#[test]
fn test_transaction_keywords() {
    assert_eq!(
        tokens("BEGIN COMMIT ROLLBACK START TRANSACTION"),
        vec![
            Token::Begin,
            Token::Commit,
            Token::Rollback,
            Token::Start,
            Token::Transaction,
            Token::Eof
        ]
    );
}

// ── Constraint keywords ───────────────────────────────────────────────────────

#[test]
fn test_constraint_keywords() {
    assert_eq!(
        tokens("PRIMARY KEY UNIQUE FOREIGN REFERENCES CHECK CONSTRAINT DEFAULT NOT"),
        vec![
            Token::Primary,
            Token::Key,
            Token::Unique,
            Token::Foreign,
            Token::References,
            Token::Check,
            Token::Constraint,
            Token::Default,
            Token::Not,
            Token::Eof,
        ]
    );
}

// ── Join keywords ─────────────────────────────────────────────────────────────

#[test]
fn test_join_keywords() {
    assert_eq!(
        tokens("JOIN INNER LEFT RIGHT FULL CROSS ON USING"),
        vec![
            Token::Join,
            Token::Inner,
            Token::Left,
            Token::Right,
            Token::Full,
            Token::Cross,
            Token::On,
            Token::Using,
            Token::Eof,
        ]
    );
}

// ── Data type keywords ────────────────────────────────────────────────────────

#[test]
fn test_data_type_keywords() {
    assert_eq!(tokens("INT"), vec![Token::TyInt, Token::Eof]);
    assert_eq!(tokens("INTEGER"), vec![Token::TyInteger, Token::Eof]);
    assert_eq!(tokens("BIGINT"), vec![Token::TyBigint, Token::Eof]);
    assert_eq!(tokens("TEXT"), vec![Token::TyText, Token::Eof]);
    assert_eq!(tokens("VARCHAR"), vec![Token::TyVarchar, Token::Eof]);
    assert_eq!(tokens("TIMESTAMP"), vec![Token::TyTimestamp, Token::Eof]);
    assert_eq!(tokens("UUID"), vec![Token::TyUuid, Token::Eof]);
}

// ── Integer literals ──────────────────────────────────────────────────────────

#[test]
fn test_integer_zero() {
    assert_eq!(tokens("0")[0], Token::Integer(0));
}

#[test]
fn test_integer_positive() {
    assert_eq!(tokens("42")[0], Token::Integer(42));
}

#[test]
fn test_integer_large() {
    assert_eq!(tokens("9223372036854775807")[0], Token::Integer(i64::MAX));
}

// ── Float literals ────────────────────────────────────────────────────────────

#[test]
fn test_float_decimal() {
    assert_eq!(tokens("3.14")[0], Token::Float(3.14));
}

#[test]
fn test_float_leading_dot() {
    assert_eq!(tokens(".5")[0], Token::Float(0.5));
}

#[test]
fn test_float_exponent_uppercase() {
    assert_eq!(tokens("1E10")[0], Token::Float(1e10));
}

#[test]
fn test_float_exponent_negative() {
    if let Token::Float(f) = tokens("1.5E-3")[0] {
        assert!((f - 0.0015).abs() < 1e-10);
    } else {
        panic!("expected Float");
    }
}

// ── String literals ───────────────────────────────────────────────────────────

#[test]
fn test_string_simple() {
    assert_eq!(tokens("'hello'")[0], Token::StringLit("hello".into()));
}

#[test]
fn test_string_empty() {
    assert_eq!(tokens("''")[0], Token::StringLit(String::new()));
}

#[test]
fn test_string_with_spaces() {
    assert_eq!(
        tokens("'hello world'")[0],
        Token::StringLit("hello world".into())
    );
}

#[test]
fn test_string_escape_newline() {
    assert_eq!(tokens(r"'\n'")[0], Token::StringLit("\n".into()));
}

#[test]
fn test_string_escape_tab() {
    assert_eq!(tokens(r"'\t'")[0], Token::StringLit("\t".into()));
}

#[test]
fn test_string_escape_carriage_return() {
    assert_eq!(tokens(r"'\r'")[0], Token::StringLit("\r".into()));
}

#[test]
fn test_string_escape_backslash() {
    assert_eq!(tokens(r"'\\'")[0], Token::StringLit("\\".into()));
}

#[test]
fn test_string_escape_single_quote() {
    assert_eq!(tokens(r"'\''")[0], Token::StringLit("'".into()));
}

#[test]
fn test_string_sql_standard_doubling() {
    // 'it''s' → "it's"
    assert_eq!(tokens("'it''s'")[0], Token::StringLit("it's".into()));
}

#[test]
fn test_string_multiple_doublings() {
    // 'a''b''c' → "a'b'c"
    assert_eq!(tokens("'a''b''c'")[0], Token::StringLit("a'b'c".into()));
}

#[test]
fn test_string_unknown_escape_passthrough() {
    // \x where x is unknown → x literally (MySQL lenient mode)
    assert_eq!(tokens(r"'\q'")[0], Token::StringLit("q".into()));
}

// ── Identifiers ───────────────────────────────────────────────────────────────

#[test]
fn test_plain_identifier() {
    assert!(matches!(&tokens("my_table")[0], Token::Ident(s) if s == "my_table"));
}

#[test]
fn test_identifier_with_digits() {
    assert!(matches!(&tokens("col1")[0], Token::Ident(s) if s == "col1"));
}

#[test]
fn test_identifier_underscore_prefix() {
    assert!(matches!(&tokens("_private")[0], Token::Ident(s) if s == "_private"));
}

#[test]
fn test_backtick_identifier() {
    assert!(matches!(&tokens("`my table`")[0], Token::QuotedIdent(s) if s == "my table"));
}

#[test]
fn test_double_quote_identifier() {
    assert!(matches!(&tokens(r#""my col""#)[0], Token::DqIdent(s) if s == "my col"));
}

// ── Operators ─────────────────────────────────────────────────────────────────

#[test]
fn test_comparison_operators() {
    assert_eq!(tokens("=")[0], Token::Eq);
    assert_eq!(tokens("<>")[0], Token::NotEq);
    assert_eq!(tokens("!=")[0], Token::NotEq);
    assert_eq!(tokens("<")[0], Token::Lt);
    assert_eq!(tokens("<=")[0], Token::LtEq);
    assert_eq!(tokens(">")[0], Token::Gt);
    assert_eq!(tokens(">=")[0], Token::GtEq);
}

#[test]
fn test_arithmetic_operators() {
    assert_eq!(tokens("+")[0], Token::Plus);
    assert_eq!(tokens("-")[0], Token::Minus);
    assert_eq!(tokens("*")[0], Token::Star);
    assert_eq!(tokens("/")[0], Token::Slash);
    assert_eq!(tokens("%")[0], Token::Percent);
}

#[test]
fn test_concat_operator() {
    assert_eq!(tokens("||")[0], Token::Concat);
}

#[test]
fn test_dot_operator() {
    assert_eq!(tokens(".")[0], Token::Dot);
}

// ── Punctuation ───────────────────────────────────────────────────────────────

#[test]
fn test_punctuation() {
    assert_eq!(tokens("(")[0], Token::LParen);
    assert_eq!(tokens(")")[0], Token::RParen);
    assert_eq!(tokens(",")[0], Token::Comma);
    assert_eq!(tokens(";")[0], Token::Semicolon);
}

// ── Comments ─────────────────────────────────────────────────────────────────

#[test]
fn test_line_comment_dashdash() {
    assert_eq!(
        tokens("-- comment\nSELECT"),
        vec![Token::Select, Token::Eof]
    );
}

#[test]
fn test_line_comment_hash() {
    assert_eq!(tokens("#comment\nSELECT"), vec![Token::Select, Token::Eof]);
}

#[test]
fn test_block_comment() {
    assert_eq!(
        tokens("/* block */ SELECT"),
        vec![Token::Select, Token::Eof]
    );
}

#[test]
fn test_block_comment_multiline() {
    assert_eq!(
        tokens("/* line1\nline2 */ SELECT"),
        vec![Token::Select, Token::Eof]
    );
}

#[test]
fn test_comment_between_tokens() {
    assert_eq!(
        tokens("SELECT /* comment */ 1"),
        vec![Token::Select, Token::Integer(1), Token::Eof]
    );
}

// ── Spans ────────────────────────────────────────────────────────────────────

#[test]
fn test_span_select_keyword() {
    let st = &tokenize("SELECT", None).unwrap()[0];
    assert_eq!(st.span.start, 0);
    assert_eq!(st.span.end, 6);
}

#[test]
fn test_span_integer_literal() {
    // "SELECT 42" — integer starts at offset 7
    let tokens = tokenize("SELECT 42", None).unwrap();
    let int_tok = &tokens[1];
    assert_eq!(int_tok.token, Token::Integer(42));
    assert_eq!(int_tok.span.start, 7);
    assert_eq!(int_tok.span.end, 9);
}

#[test]
fn test_span_eof_at_end_of_input() {
    let input = "SELECT";
    let tokens = tokenize(input, None).unwrap();
    let eof = tokens.last().unwrap();
    assert_eq!(eof.token, Token::Eof);
    assert_eq!(eof.span.start, input.len());
    assert_eq!(eof.span.end, input.len());
}

// ── Sentinel ─────────────────────────────────────────────────────────────────

#[test]
fn test_empty_input_eof_only() {
    assert_eq!(tokens(""), vec![Token::Eof]);
}

#[test]
fn test_whitespace_only_eof_only() {
    assert_eq!(tokens("   \t\n  "), vec![Token::Eof]);
}

#[test]
fn test_eof_always_last() {
    let toks = tokens("SELECT 1");
    assert_eq!(*toks.last().unwrap(), Token::Eof);
}

#[test]
fn test_semicolon_multi_statement() {
    assert_eq!(
        tokens("SELECT 1; SELECT 2"),
        vec![
            Token::Select,
            Token::Integer(1),
            Token::Semicolon,
            Token::Select,
            Token::Integer(2),
            Token::Eof,
        ]
    );
}

// ── Error cases (4.2b) ────────────────────────────────────────────────────────

#[test]
fn test_error_unexpected_at_sign() {
    let e = tokens_err("@");
    assert!(matches!(e, DbError::ParseError { .. }));
}

#[test]
fn test_error_unexpected_caret() {
    let e = tokens_err("^");
    assert!(matches!(e, DbError::ParseError { .. }));
}

#[test]
fn test_error_unexpected_dollar() {
    let e = tokens_err("$");
    assert!(matches!(e, DbError::ParseError { .. }));
}

#[test]
fn test_error_unexpected_char_position_in_message() {
    let e = tokens_err("SELECT @");
    if let DbError::ParseError { message } = e {
        assert!(
            message.contains('@'),
            "error message should contain '@': {message}"
        );
    } else {
        panic!("expected ParseError");
    }
}

#[test]
fn test_error_query_too_long() {
    let input = "SELECT 1".repeat(200); // 1600 bytes
    let e = tokenize(&input, Some(100)).unwrap_err();
    assert!(matches!(e, DbError::ParseError { .. }));
}

#[test]
fn test_no_error_within_limit() {
    let input = "SELECT 1";
    assert!(tokenize(input, Some(input.len())).is_ok());
}

#[test]
fn test_never_panics_empty() {
    assert!(tokenize("", None).is_ok());
}

#[test]
fn test_never_panics_only_whitespace() {
    assert!(tokenize("    \t\n   ", None).is_ok());
}

#[test]
fn test_never_panics_unicode_text() {
    // Unicode in string literals is fine
    assert!(tokenize("'こんにちは 🦀'", None).is_ok());
}

// ── Full queries ──────────────────────────────────────────────────────────────

#[test]
fn test_full_select_query() {
    let toks = tokens("SELECT id, name FROM users WHERE age > 18 ORDER BY name ASC LIMIT 10");
    assert_eq!(toks[0], Token::Select);
    assert!(matches!(&toks[1], Token::Ident(s) if s == "id"));
    assert_eq!(toks[2], Token::Comma);
    assert!(matches!(&toks[3], Token::Ident(s) if s == "name"));
    assert_eq!(toks[4], Token::From);
    assert!(matches!(&toks[5], Token::Ident(s) if s == "users"));
    assert_eq!(toks[6], Token::Where);
    assert!(matches!(&toks[7], Token::Ident(s) if s == "age"));
    assert_eq!(toks[8], Token::Gt);
    assert_eq!(toks[9], Token::Integer(18));
    assert_eq!(toks[10], Token::Order);
    assert_eq!(toks[11], Token::By);
    assert!(matches!(&toks[12], Token::Ident(s) if s == "name"));
    assert_eq!(toks[13], Token::Asc);
    assert_eq!(toks[14], Token::Limit);
    assert_eq!(toks[15], Token::Integer(10));
    assert_eq!(*toks.last().unwrap(), Token::Eof);
}

#[test]
fn test_full_insert_query() {
    let toks = tokens("INSERT INTO users (id, name) VALUES (1, 'Alice')");
    assert_eq!(toks[0], Token::Insert);
    assert_eq!(toks[1], Token::Into);
    assert!(matches!(&toks[2], Token::Ident(s) if s == "users"));
    assert_eq!(toks[3], Token::LParen);
    assert_eq!(toks[5], Token::Comma);
    assert_eq!(toks[7], Token::RParen);
    assert_eq!(toks[8], Token::Values);
    assert_eq!(toks[9], Token::LParen);
    assert_eq!(toks[10], Token::Integer(1));
    assert_eq!(toks[11], Token::Comma);
    assert_eq!(toks[12], Token::StringLit("Alice".into()));
    assert_eq!(toks[13], Token::RParen);
}

#[test]
fn test_create_table_query() {
    let toks = tokens("CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT NOT NULL)");
    assert_eq!(toks[0], Token::Create);
    assert_eq!(toks[1], Token::Table);
    assert!(matches!(&toks[2], Token::Ident(s) if s == "users"));
    assert_eq!(toks[3], Token::LParen);
    assert!(matches!(&toks[4], Token::Ident(s) if s == "id"));
    assert_eq!(toks[5], Token::TyBigint);
    assert_eq!(toks[6], Token::Primary);
    assert_eq!(toks[7], Token::Key);
    assert_eq!(toks[8], Token::Comma);
    assert!(matches!(&toks[9], Token::Ident(s) if s == "name"));
    assert_eq!(toks[10], Token::TyText);
    assert_eq!(toks[11], Token::Not);
    assert_eq!(toks[12], Token::Null);
    assert_eq!(toks[13], Token::RParen);
}
