//! REPL (Read-Eval-Print Loop) for the interactive CLI.
//!
//! Uses [`rustyline`] in interactive (TTY) mode for:
//! - Arrow-key navigation (← →) within the current line
//! - Command history with ↑ / ↓ (persisted in `~/.axiomdb_history`)
//! - Ctrl-A / Ctrl-E to jump to start/end of line
//! - Ctrl-R for reverse-history search
//! - SQL keyword completion (Tab)
//!
//! Falls back to plain `stdin` line-by-line reading when stdin is not a TTY
//! (pipe/file mode), so scripts and pipes still work without any readline overhead.
//!
//! ## Multi-line queries
//!
//! Lines are accumulated until a line ending with `;` (trimmed) is seen.
//! The secondary prompt `   -> ` signals that more input is expected.
//!
//! ## Dot commands
//!
//! Dispatched when the buffer is empty and the line starts with `.` or is `\q`.

use std::time::Instant;

use rustyline::completion::{Completer, FilenameCompleter, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::HistoryHinter;
use rustyline::validate::Validator;
use rustyline::{CompletionType, Config, Context, Editor, Helper};

use axiomdb_network::mysql::Database;
use axiomdb_sql::{result::QueryResult, SchemaCache, SessionContext};

use crate::dot::{self, DotResult};
use crate::table::format_table;

// ── SQL keyword completer ─────────────────────────────────────────────────────

/// Keywords shown as Tab-completion candidates.
const SQL_KEYWORDS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "INSERT",
    "INTO",
    "VALUES",
    "UPDATE",
    "SET",
    "DELETE",
    "CREATE",
    "TABLE",
    "DROP",
    "ALTER",
    "ADD",
    "COLUMN",
    "INDEX",
    "UNIQUE",
    "PRIMARY",
    "KEY",
    "REFERENCES",
    "ON",
    "CASCADE",
    "RESTRICT",
    "NOT",
    "NULL",
    "DEFAULT",
    "AUTO_INCREMENT",
    "CONSTRAINT",
    "CHECK",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "SHOW",
    "TABLES",
    "COLUMNS",
    "DESCRIBE",
    "TRUNCATE",
    "JOIN",
    "LEFT",
    "RIGHT",
    "INNER",
    "OUTER",
    "CROSS",
    "GROUP",
    "BY",
    "ORDER",
    "ASC",
    "DESC",
    "LIMIT",
    "OFFSET",
    "HAVING",
    "DISTINCT",
    "AS",
    "AND",
    "OR",
    "IN",
    "IS",
    "BETWEEN",
    "LIKE",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "COUNT",
    "SUM",
    "AVG",
    "MIN",
    "MAX",
    "INT",
    "BIGINT",
    "TEXT",
    "BOOL",
    "REAL",
    "TIMESTAMP",
    "UUID",
    "BYTES",
    ".tables",
    ".schema",
    ".quit",
    ".exit",
    ".open",
    ".help",
];

struct SqlHelper {
    file_completer: FilenameCompleter,
    hinter: HistoryHinter,
}

impl SqlHelper {
    fn new() -> Self {
        Self {
            file_completer: FilenameCompleter::new(),
            hinter: HistoryHinter::new(),
        }
    }
}

impl Helper for SqlHelper {}
impl Validator for SqlHelper {}
impl Highlighter for SqlHelper {}

impl rustyline::hint::Hinter for SqlHelper {
    type Hint = String;
    fn hint(&self, line: &str, pos: usize, ctx: &Context<'_>) -> Option<String> {
        self.hinter.hint(line, pos, ctx)
    }
}

impl Completer for SqlHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        // Find the start of the current word being typed.
        let word_start = line[..pos]
            .rfind(|c: char| c.is_whitespace() || c == '(' || c == ',')
            .map(|i| i + 1)
            .unwrap_or(0);
        let prefix = &line[word_start..pos];

        if prefix.is_empty() {
            return Ok((pos, vec![]));
        }

        let prefix_upper = prefix.to_ascii_uppercase();
        let candidates: Vec<Pair> = SQL_KEYWORDS
            .iter()
            .filter(|kw| kw.to_ascii_uppercase().starts_with(&prefix_upper))
            .map(|kw| Pair {
                display: kw.to_string(),
                replacement: kw.to_string(),
            })
            .collect();

        if !candidates.is_empty() {
            return Ok((word_start, candidates));
        }

        // Fall back to filename completion for .open paths.
        if line.trim_start().starts_with(".open") {
            return self.file_completer.complete(line, pos, _ctx);
        }

        Ok((pos, vec![]))
    }
}

// ── History file ─────────────────────────────────────────────────────────────

/// Returns the path for the persistent history file.
///
/// Uses `~/.axiomdb_history` (cross-platform via `HOME` / `USERPROFILE`).
fn history_path() -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    Some(std::path::PathBuf::from(home).join(".axiomdb_history"))
}

// ── REPL entry point ─────────────────────────────────────────────────────────

/// Runs the interactive REPL until the user quits or EOF is reached.
///
/// Uses `rustyline` when stdin is a TTY for history and completion. Falls back
/// to plain `stdin` line-by-line reading in pipe/script mode.
pub fn run(db: &mut Database) {
    if std::io::stdin().is_terminal() {
        run_interactive(db);
    } else {
        run_pipe(db);
    }
}

/// Interactive mode: rustyline with history + SQL completion.
fn run_interactive(db: &mut Database) {
    let config = Config::builder()
        .history_ignore_space(true)
        .completion_type(CompletionType::List)
        .build();

    let mut rl: Editor<SqlHelper, _> = match Editor::with_config(config) {
        Ok(e) => e,
        Err(_) => {
            // rustyline failed to init — fall back to pipe mode silently.
            run_pipe(db);
            return;
        }
    };
    rl.set_helper(Some(SqlHelper::new()));

    // Load history from disk (ignore errors — first run has no file).
    if let Some(path) = history_path() {
        let _ = rl.load_history(&path);
    }

    let mut buffer = String::new();

    loop {
        let prompt = if buffer.trim().is_empty() {
            "axiomdb> "
        } else {
            "   -> "
        };

        match rl.readline(prompt) {
            Ok(line) => {
                let trimmed = line.trim_end();

                if trimmed.is_empty() && buffer.trim().is_empty() {
                    continue;
                }

                // Dot commands (only when no SQL is buffered).
                if buffer.trim().is_empty() {
                    let lower = trimmed.trim().to_ascii_lowercase();
                    if lower.starts_with('.')
                        || lower == "\\q"
                        || lower.starts_with(".quit")
                        || lower.starts_with(".exit")
                    {
                        // Add to history even for dot commands.
                        let _ = rl.add_history_entry(trimmed);
                        match dot::handle(trimmed.trim(), db) {
                            DotResult::Ok => continue,
                            DotResult::Quit => break,
                            DotResult::Error(e) => {
                                eprintln!("Error: {e}");
                                continue;
                            }
                        }
                    }
                }

                buffer.push_str(trimmed);
                buffer.push('\n');

                if trimmed.ends_with(';') {
                    let sql = std::mem::take(&mut buffer);
                    // Add the complete statement to history (single line for readability).
                    let history_entry = sql.trim().replace('\n', " ");
                    let _ = rl.add_history_entry(&history_entry);
                    execute_sql(sql.trim(), db, true);
                }
            }

            Err(ReadlineError::Interrupted) => {
                // Ctrl-C: clear the current buffer, restart prompt.
                if !buffer.is_empty() {
                    buffer.clear();
                    println!("^C");
                } else {
                    // Second Ctrl-C with empty buffer → exit.
                    println!("^C (use .quit or Ctrl-D to exit)");
                }
            }

            Err(ReadlineError::Eof) => {
                // Ctrl-D: exit cleanly.
                println!();
                break;
            }

            Err(e) => {
                eprintln!("axiomdb-cli: readline error: {e}");
                break;
            }
        }
    }

    // Save history to disk.
    if let Some(path) = history_path() {
        let _ = rl.save_history(&path);
    }

    println!("Bye.");
}

/// Pipe / script mode: plain stdin reading, no prompt, no history.
///
/// Reads all of stdin, then splits on `;` (respecting quoted strings) so that
/// multiple statements on a single line or across multiple lines all execute.
fn run_pipe(db: &mut Database) {
    use std::io::Read;

    let mut input = String::new();
    if std::io::stdin().lock().read_to_string(&mut input).is_err() {
        return;
    }

    // Split on ';' while respecting single-quoted strings.
    for stmt in split_stmts(&input) {
        execute_sql(stmt, db, false);
    }
}

/// Splits SQL input on `;` delimiters, returning non-empty trimmed statements.
/// Handles single-quoted string literals (same logic as handler.rs).
fn split_stmts(input: &str) -> Vec<&str> {
    let mut stmts = Vec::new();
    let mut start = 0usize;
    let mut in_string = false;
    let bytes = input.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            b'\'' if !in_string => {
                in_string = true;
                i += 1;
            }
            b'\'' if in_string => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    i += 2;
                } else {
                    in_string = false;
                    i += 1;
                }
            }
            b'\\' if in_string => {
                i += 2;
            }
            b';' if !in_string => {
                let stmt = input[start..i].trim();
                if !stmt.is_empty() {
                    stmts.push(stmt);
                }
                start = i + 1;
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }
    let tail = input[start..].trim();
    if !tail.is_empty() {
        stmts.push(tail);
    }
    stmts
}

/// Executes a single SQL statement and prints the result.
fn execute_sql(sql: &str, db: &mut Database, is_tty: bool) {
    if sql.is_empty() {
        return;
    }

    let t0 = Instant::now();
    let mut schema_cache = SchemaCache::new();
    let mut session = SessionContext::new();

    match db.execute_query(sql, &mut session, &mut schema_cache) {
        Ok((result, _commit_rx)) => {
            let elapsed = t0.elapsed();
            match result {
                QueryResult::Rows { columns, rows } => {
                    print!("{}", format_table(&columns, &rows, elapsed));
                }
                QueryResult::Affected {
                    count,
                    last_insert_id,
                } => {
                    let ms = elapsed.as_millis();
                    let row_word = if count == 1 { "row" } else { "rows" };
                    if let Some(id) = last_insert_id {
                        println!("{count} {row_word} affected, last_insert_id={id} ({ms}ms)");
                    } else {
                        println!("{count} {row_word} affected ({ms}ms)");
                    }
                }
                QueryResult::Empty => {
                    println!("OK ({}ms)", elapsed.as_millis());
                }
            }
        }
        Err(e) => {
            eprintln!("Error [{}]: {}", e.sqlstate(), e);
        }
    }

    if is_tty {
        println!();
    }
}

// Trait import for is_terminal() — needed for TTY detection.
use std::io::IsTerminal as _;
