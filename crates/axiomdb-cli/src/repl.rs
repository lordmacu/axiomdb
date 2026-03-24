//! REPL (Read-Eval-Print Loop) for the interactive CLI.
//!
//! ## Input handling
//!
//! - Lines are read from `stdin` one at a time.
//! - Input is accumulated until a line ending with `;` (after trimming) is seen,
//!   at which point the accumulated SQL is executed.
//! - Dot commands (`.tables`, `.quit`, etc.) are dispatched immediately when the
//!   buffer is empty, before any SQL parsing.
//! - On EOF (Ctrl-D), the loop exits cleanly.
//!
//! ## TTY detection
//!
//! When stdin is a TTY (interactive terminal), the prompt is printed before each
//! line. When stdin is a pipe or file (non-interactive), no prompt is printed so
//! that only result output appears in the pipe.

use std::io::IsTerminal as _;
use std::io::{BufRead, Write};
use std::time::Instant;

use axiomdb_network::mysql::Database;
use axiomdb_sql::{SchemaCache, SessionContext};

use crate::dot::{self, DotResult};
use crate::table::format_table;

/// Runs the interactive REPL until the user quits or EOF is reached.
pub fn run(db: &mut Database) {
    let is_tty = std::io::stdin().is_terminal();
    let stdin = std::io::stdin();
    let mut buffer = String::new(); // accumulated multi-line SQL

    loop {
        // Print the prompt only in interactive (TTY) mode.
        if is_tty {
            let prompt = if buffer.trim().is_empty() {
                "axiomdb> "
            } else {
                "   -> "
            };
            print!("{prompt}");
            std::io::stdout().flush().ok();
        }

        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => {
                // EOF — exit cleanly.
                if is_tty {
                    println!(); // blank line after Ctrl-D
                }
                break;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("axiomdb-cli: read error: {e}");
                break;
            }
        }

        let trimmed = line.trim_end();

        // Ignore empty lines at the top level (no buffered content yet).
        if trimmed.is_empty() && buffer.trim().is_empty() {
            continue;
        }

        // Dot commands are only valid when no SQL is being accumulated.
        if buffer.trim().is_empty() {
            let lower = trimmed.trim().to_ascii_lowercase();
            if lower.starts_with('.')
                || lower == "\\q"
                || lower.starts_with(".quit")
                || lower.starts_with(".exit")
            {
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

        // Accumulate the line.
        buffer.push_str(trimmed);
        buffer.push('\n');

        // Execute when the line ends with ';' (statement terminator).
        if trimmed.ends_with(';') {
            let sql = std::mem::take(&mut buffer);
            execute_sql(sql.trim(), db, is_tty);
        }
    }

    if is_tty {
        println!("Bye.");
    }
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
            // _commit_rx is ignored — CLI commits are always synchronous.
            let elapsed = t0.elapsed();
            use axiomdb_sql::result::QueryResult;
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
                    let ms = elapsed.as_millis();
                    println!("OK ({ms}ms)");
                }
            }
        }
        Err(e) => {
            // Print errors to stderr in both interactive and pipe modes.
            // Use SQLSTATE code for structured error display like psql.
            eprintln!("Error [{}]: {}", e.sqlstate(), e);
        }
    }

    // In interactive mode, print a blank line after each result for readability.
    if is_tty {
        println!();
    }
}
