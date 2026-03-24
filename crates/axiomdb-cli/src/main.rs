//! # axiomdb-cli — Interactive SQL shell for AxiomDB
//!
//! Connects directly to a local database file (no server, no network) and
//! provides an interactive REPL similar to `sqlite3` or `psql`.
//!
//! ## Usage
//!
//! ```bash
//! axiomdb-cli [path]          # open database at path (default: ./axiomdb.db)
//! axiomdb-cli --help          # show usage
//!
//! # Pipe mode (no prompt printed):
//! echo "SELECT 1;" | axiomdb-cli ./data.db
//! axiomdb-cli ./data.db < migration.sql
//! ```
//!
//! ## Interactive commands
//!
//! SQL statements (end with `;`) are executed immediately.
//! Multi-line input is accumulated until the `;` terminator.
//!
//! Dot commands:
//! ```
//! .help               Show available commands
//! .tables             List all user tables
//! .schema [table]     Show columns and indexes
//! .open <path>        Switch to a different database file
//! .quit               Exit (also: .exit, \q, Ctrl-D)
//! ```

mod dot;
mod repl;
mod table;

use std::io::IsTerminal as _;

use axiomdb_network::mysql::Database;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = match args.next() {
        Some(p) if p == "--help" || p == "-h" => {
            print_usage();
            return;
        }
        Some(p) => p,
        None => "./axiomdb.db".to_string(),
    };

    let data_path = std::path::Path::new(&path);

    let mut db = match Database::open(data_path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("axiomdb-cli: failed to open '{}': {}", path, e);
            std::process::exit(1);
        }
    };

    // Print the welcome banner only in interactive (TTY) mode.
    if std::io::stdin().is_terminal() {
        println!("AxiomDB {} — interactive shell", env!("CARGO_PKG_VERSION"));
        println!("Database: {}", data_path.display());
        println!("Type SQL ending with ; to execute. Type .help for commands.");
        println!();
    }

    repl::run(&mut db);
}

fn print_usage() {
    println!("Usage: axiomdb-cli [path]");
    println!();
    println!("  path    Path to the database file (default: ./axiomdb.db)");
    println!();
    println!("Interactive commands:");
    println!("  .help               Show available commands");
    println!("  .tables             List all user tables");
    println!("  .schema [table]     Show columns and indexes");
    println!("  .open <path>        Switch to a different database file");
    println!("  .quit               Exit (also: .exit, \\q, Ctrl-D)");
    println!();
    println!("Pipe mode:");
    println!("  echo \"SELECT 1;\" | axiomdb-cli ./data.db");
    println!("  axiomdb-cli ./data.db < migration.sql");
}
