//! Dot command handlers for the interactive CLI.
//!
//! Dot commands are meta-commands that control the CLI itself rather than the
//! database. They begin with `.` (or `\q` for quit) and are handled before SQL
//! parsing. Examples: `.tables`, `.schema`, `.quit`, `.help`.

use axiomdb_network::mysql::Database;
use axiomdb_sql::{result::QueryResult, SchemaCache, SessionContext};
use axiomdb_types::Value;

/// Result of handling a dot command.
pub enum DotResult {
    /// Command executed successfully — continue the REPL loop.
    Ok,
    /// User requested to exit — break the REPL loop.
    Quit,
    /// Command failed — print error and continue.
    Error(String),
}

/// Dispatches a dot command string to its handler.
///
/// `cmd` is the full line with leading `.` included (e.g. `".tables"` or
/// `".schema users"`). Matching is case-insensitive.
pub fn handle(cmd: &str, db: &mut Database) -> DotResult {
    let cmd = cmd.trim();
    let (name, arg) = if let Some(pos) = cmd.find(|c: char| c.is_whitespace()) {
        (&cmd[..pos], cmd[pos..].trim())
    } else {
        (cmd, "")
    };

    match name.to_ascii_lowercase().as_str() {
        ".quit" | ".exit" | "\\q" => DotResult::Quit,

        ".help" => {
            println!("Dot commands:");
            println!("  .help               Show this message");
            println!("  .tables             List all user tables");
            println!("  .schema [table]     Show columns and indexes (all if omitted)");
            println!("  .open <path>        Open a different database file");
            println!("  .quit               Exit  (also: .exit, \\q, Ctrl-D)");
            DotResult::Ok
        }

        ".tables" => {
            let mut session = SessionContext::new();
            let mut cache = SchemaCache::new();
            match db.execute_query("SHOW TABLES", &mut session, &mut cache) {
                Ok((QueryResult::Rows { rows, .. }, _)) => {
                    let mut names: Vec<String> = rows
                        .iter()
                        .filter_map(|r| r.first())
                        .filter_map(|v| {
                            if let Value::Text(s) = v {
                                Some(s.clone())
                            } else {
                                None
                            }
                        })
                        .collect();
                    names.sort();
                    if names.is_empty() {
                        println!("(no tables)");
                    } else {
                        for name in names {
                            println!("{name}");
                        }
                    }
                    DotResult::Ok
                }
                Ok(_) => DotResult::Ok,
                Err(e) => DotResult::Error(e.to_string()),
            }
        }

        ".schema" => {
            let table_filter = if arg.is_empty() { None } else { Some(arg) };
            print_schema(db, table_filter)
        }

        ".open" => {
            if arg.is_empty() {
                return DotResult::Error(".open requires a path".into());
            }
            match Database::open(std::path::Path::new(arg)) {
                Ok(new_db) => {
                    *db = new_db;
                    println!("Opened '{arg}'");
                    DotResult::Ok
                }
                Err(e) => DotResult::Error(format!("failed to open '{arg}': {e}")),
            }
        }

        other => DotResult::Error(format!("unknown command '{other}' — type .help")),
    }
}

/// Prints the schema for one table (or all tables if `table_filter` is None).
fn print_schema(db: &mut Database, table_filter: Option<&str>) -> DotResult {
    // Get the list of tables to describe.
    let mut session = SessionContext::new();
    let mut cache = SchemaCache::new();

    let tables: Vec<String> = match db.execute_query("SHOW TABLES", &mut session, &mut cache) {
        Ok((QueryResult::Rows { rows, .. }, _)) => {
            let mut names: Vec<String> = rows
                .iter()
                .filter_map(|r| r.first())
                .filter_map(|v| {
                    if let Value::Text(s) = v {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
                .collect();
            names.sort();
            names
        }
        Ok(_) => return DotResult::Ok,
        Err(e) => return DotResult::Error(e.to_string()),
    };

    for table_name in &tables {
        // If a filter is given, skip non-matching tables.
        if let Some(filter) = table_filter {
            if !table_name.eq_ignore_ascii_case(filter) {
                continue;
            }
        }

        println!("Table: {table_name}");

        // SHOW COLUMNS for this table.
        let sql = format!("SHOW COLUMNS FROM `{table_name}`");
        let mut session2 = SessionContext::new();
        let mut cache2 = SchemaCache::new();
        match db.execute_query(&sql, &mut session2, &mut cache2) {
            Ok((QueryResult::Rows { rows, columns, .. }, _)) => {
                // Expected columns: Field, Type, Null, Key, Default, Extra
                let field_idx = columns.iter().position(|c| c.name == "Field").unwrap_or(0);
                let type_idx = columns.iter().position(|c| c.name == "Type").unwrap_or(1);
                let null_idx = columns.iter().position(|c| c.name == "Null").unwrap_or(2);
                let extra_idx = columns.iter().position(|c| c.name == "Extra").unwrap_or(5);

                for row in &rows {
                    let field = text_at(row, field_idx);
                    let typ = text_at(row, type_idx);
                    let nullable = text_at(row, null_idx) == "YES";
                    let extra = text_at(row, extra_idx);

                    let null_str = if nullable { "nullable" } else { "NOT NULL" };
                    let extra_str = if extra.is_empty() {
                        String::new()
                    } else {
                        format!("  {extra}")
                    };
                    println!("  {field:<20} {typ:<12} {null_str}{extra_str}");
                }
            }
            Ok(_) => {}
            Err(e) => println!("  (error reading columns: {e})"),
        }

        // SHOW INDEX for this table (print if any).
        let sql = format!("SHOW INDEX FROM `{table_name}`");
        let mut session3 = SessionContext::new();
        let mut cache3 = SchemaCache::new();
        if let Ok((QueryResult::Rows { rows, columns, .. }, _)) =
            db.execute_query(&sql, &mut session3, &mut cache3)
        {
            if !rows.is_empty() {
                println!("  Indexes:");
                let name_idx = columns
                    .iter()
                    .position(|c| c.name == "Key_name")
                    .unwrap_or(2);
                let col_idx = columns
                    .iter()
                    .position(|c| c.name == "Column_name")
                    .unwrap_or(4);
                let unique_idx = columns
                    .iter()
                    .position(|c| c.name == "Non_unique")
                    .unwrap_or(1);

                // Group by index name
                let mut idx_map: std::collections::BTreeMap<String, (Vec<String>, bool)> =
                    std::collections::BTreeMap::new();
                for row in &rows {
                    let iname = text_at(row, name_idx);
                    let col = text_at(row, col_idx);
                    let non_unique = text_at(row, unique_idx) == "1";
                    let entry = idx_map.entry(iname).or_insert((Vec::new(), !non_unique));
                    entry.0.push(col);
                }
                for (iname, (icols, is_unique)) in &idx_map {
                    let unique_str = if *is_unique { " UNIQUE" } else { "" };
                    println!("    {iname} ({}) {unique_str}", icols.join(", "));
                }
            }
        }

        println!(); // blank line between tables
    }

    DotResult::Ok
}

/// Extracts a text value at `idx` from a row, or returns an empty string.
fn text_at(row: &[Value], idx: usize) -> String {
    row.get(idx)
        .and_then(|v| {
            if let Value::Text(s) = v {
                Some(s.clone())
            } else {
                None
            }
        })
        .unwrap_or_default()
}
