//! Schema migrations CLI — `axiomdb-server migrate` subcommand (Phase 22b.5).
//!
//! ## Usage
//!
//! ```bash
//! axiomdb-server migrate status   [--data-dir DIR] [--db DATABASE] [--dir MIGRATIONS]
//! axiomdb-server migrate up       [--data-dir DIR] [--db DATABASE] [--dir MIGRATIONS]
//! axiomdb-server migrate down     [--data-dir DIR] [--db DATABASE] [--dir MIGRATIONS]
//! axiomdb-server migrate create   [--dir MIGRATIONS] NAME
//! ```
//!
//! ## Migration file format
//!
//! Files are named `{N}_{description}.sql` (e.g., `001_create_users.sql`).
//! Each file may contain an optional DOWN section:
//!
//! ```sql
//! CREATE TABLE users (id INT PRIMARY KEY, name TEXT);
//!
//! -- +migrate Down
//! DROP TABLE users;
//! ```
//!
//! If no `-- +migrate Down` marker is present, only UP is available.
//!
//! ## State tracking
//!
//! Applied migrations are recorded in `axiomdb_migrations` table inside
//! the target database. The table is created automatically on first use.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axiomdb_network::mysql::SharedDatabase;
use axiomdb_sql::{session::SessionContext, SchemaCache};

// ── Migration file ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Migration {
    pub version: u32,
    pub name: String,
    pub up_sql: String,
    pub down_sql: Option<String>,
    #[allow(dead_code)]
    pub file_path: PathBuf,
}

const DOWN_MARKER: &str = "-- +migrate Down";

impl Migration {
    pub fn from_file(path: &Path) -> Result<Self, String> {
        let filename = path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| format!("invalid filename: {}", path.display()))?;

        // Parse `{version}_{description}` from filename.
        let (version_str, description) = filename
            .split_once('_')
            .ok_or_else(|| format!("migration filename must be `N_description.sql`: {filename}"))?;

        let version: u32 = version_str
            .parse()
            .map_err(|_| format!("migration version must be a number, got '{version_str}'"))?;

        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;

        let (up_sql, down_sql) = if let Some(idx) = content.find(DOWN_MARKER) {
            let up = content[..idx].trim().to_string();
            let down = content[idx + DOWN_MARKER.len()..].trim().to_string();
            (up, if down.is_empty() { None } else { Some(down) })
        } else {
            (content.trim().to_string(), None)
        };

        Ok(Migration {
            version,
            name: description.to_string(),
            up_sql,
            down_sql,
            file_path: path.to_path_buf(),
        })
    }

    pub fn label(&self) -> String {
        format!("{:04}_{}", self.version, self.name)
    }
}

// ── Migration loader ──────────────────────────────────────────────────────────

pub fn load_migrations(dir: &Path) -> Result<Vec<Migration>, String> {
    if !dir.exists() {
        return Err(format!(
            "migrations directory '{}' does not exist",
            dir.display()
        ));
    }

    let mut migrations: Vec<Migration> = std::fs::read_dir(dir)
        .map_err(|e| format!("cannot read migrations dir: {e}"))?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension()?.to_str()? == "sql" {
                Some(path)
            } else {
                None
            }
        })
        .map(|path| Migration::from_file(&path))
        .collect::<Result<Vec<_>, _>>()?;

    migrations.sort_by_key(|m| m.version);

    // Reject duplicate version numbers.
    for i in 1..migrations.len() {
        if migrations[i].version == migrations[i - 1].version {
            return Err(format!(
                "duplicate migration version {}: '{}' and '{}'",
                migrations[i].version,
                migrations[i - 1].label(),
                migrations[i].label(),
            ));
        }
    }

    Ok(migrations)
}

// ── Database executor ─────────────────────────────────────────────────────────

fn exec_sql(db: &SharedDatabase, database: &str, sql: &str) -> Result<(), String> {
    let mut ctx = SessionContext::new();
    ctx.set_current_database(database.to_string());
    let mut schema_cache = SchemaCache::new();
    let statements: Vec<&str> = split_statements(sql);
    for stmt in statements {
        let stmt = stmt.trim();
        if stmt.is_empty() {
            continue;
        }
        db.execute_query(stmt, &mut ctx, &mut schema_cache)
            .map_err(|e| format!("SQL error: {e}\n  Statement: {stmt}"))?;
    }
    Ok(())
}

fn query_rows(
    db: &SharedDatabase,
    database: &str,
    sql: &str,
) -> Result<Vec<Vec<axiomdb_types::Value>>, String> {
    use axiomdb_sql::QueryResult;
    let mut ctx = SessionContext::new();
    ctx.set_current_database(database.to_string());
    let mut schema_cache = SchemaCache::new();
    let (result, _) = db
        .execute_query(sql, &mut ctx, &mut schema_cache)
        .map_err(|e| format!("SQL error: {e}"))?;
    match result {
        QueryResult::Rows { rows, .. } => Ok(rows),
        _ => Ok(vec![]),
    }
}

fn split_statements(sql: &str) -> Vec<&str> {
    // Split on `;` that are not inside string literals.
    // Simple heuristic: split on `;` followed by whitespace/newline/EOF.
    let mut stmts = Vec::new();
    let mut start = 0;
    let bytes = sql.as_bytes();
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' if !in_double => {
                // handle escaped quotes ''
                if in_single && i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    i += 2;
                    continue;
                }
                in_single = !in_single;
            }
            b'"' if !in_single => {
                in_double = !in_double;
            }
            b';' if !in_single && !in_double => {
                let stmt = &sql[start..i];
                stmts.push(stmt);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    let tail = sql[start..].trim();
    if !tail.is_empty() {
        stmts.push(tail);
    }
    stmts
}

// ── Migration state table ─────────────────────────────────────────────────────

const CREATE_MIGRATIONS_TABLE: &str = "
CREATE TABLE IF NOT EXISTS axiomdb_migrations (
    version     INT          NOT NULL,
    name        TEXT         NOT NULL,
    applied_at  BIGINT       NOT NULL,
    PRIMARY KEY (version)
)";

fn ensure_migrations_table(db: &SharedDatabase, database: &str) -> Result<(), String> {
    exec_sql(db, database, CREATE_MIGRATIONS_TABLE)
}

fn applied_versions(db: &SharedDatabase, database: &str) -> Result<Vec<(u32, String)>, String> {
    ensure_migrations_table(db, database)?;
    let rows = query_rows(
        db,
        database,
        "SELECT version, name FROM axiomdb_migrations ORDER BY version",
    )?;
    rows.into_iter()
        .map(|row| {
            let version = match &row[0] {
                axiomdb_types::Value::Int(n) => *n as u32,
                axiomdb_types::Value::BigInt(n) => *n as u32,
                other => {
                    return Err(format!("unexpected version type: {other:?}"));
                }
            };
            let name = match &row[1] {
                axiomdb_types::Value::Text(s) => s.clone(),
                other => return Err(format!("unexpected name type: {other:?}")),
            };
            Ok((version, name))
        })
        .collect()
}

fn record_applied(db: &SharedDatabase, database: &str, m: &Migration) -> Result<(), String> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let sql = format!(
        "INSERT INTO axiomdb_migrations (version, name, applied_at) VALUES ({}, '{}', {})",
        m.version,
        m.name.replace('\'', "''"),
        now_ms,
    );
    exec_sql(db, database, &sql)
}

fn remove_applied(db: &SharedDatabase, database: &str, version: u32) -> Result<(), String> {
    let sql = format!("DELETE FROM axiomdb_migrations WHERE version = {version}");
    exec_sql(db, database, &sql)
}

// ── Commands ──────────────────────────────────────────────────────────────────

pub fn cmd_status(db: &Arc<SharedDatabase>, database: &str, migrations_dir: &Path) {
    let migrations = match load_migrations(migrations_dir) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    let applied = match applied_versions(db, database) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    let applied_set: std::collections::HashSet<u32> = applied.iter().map(|(v, _)| *v).collect();

    if migrations.is_empty() {
        println!("No migration files found in '{}'", migrations_dir.display());
        return;
    }

    println!("{:<8} {:<40} Status", "Version", "Name");
    println!("{}", "-".repeat(60));
    for m in &migrations {
        let status = if applied_set.contains(&m.version) {
            "applied"
        } else {
            "pending"
        };
        println!("{:<8} {:<40} {}", m.version, m.name, status);
    }

    let pending = migrations
        .iter()
        .filter(|m| !applied_set.contains(&m.version))
        .count();
    println!();
    println!(
        "{} migrations total, {} applied, {} pending",
        migrations.len(),
        applied_set.len(),
        pending,
    );
}

pub fn cmd_up(db: &Arc<SharedDatabase>, database: &str, migrations_dir: &Path) {
    let migrations = match load_migrations(migrations_dir) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    let applied = match applied_versions(db, database) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    let applied_set: std::collections::HashSet<u32> = applied.iter().map(|(v, _)| *v).collect();

    let pending: Vec<&Migration> = migrations
        .iter()
        .filter(|m| !applied_set.contains(&m.version))
        .collect();

    if pending.is_empty() {
        println!("Nothing to migrate. All migrations applied.");
        return;
    }

    for m in pending {
        print!("  Applying {:04}_{} ...", m.version, m.name);
        if let Err(e) = exec_sql(db, database, &m.up_sql) {
            println!(" FAILED");
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        if let Err(e) = record_applied(db, database, m) {
            println!(" FAILED (recording state)");
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        println!(" ok");
    }

    println!("Done.");
}

pub fn cmd_down(db: &Arc<SharedDatabase>, database: &str, migrations_dir: &Path) {
    let migrations = match load_migrations(migrations_dir) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    let applied = match applied_versions(db, database) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    let last = match applied.last() {
        Some(v) => v.clone(),
        None => {
            println!("No applied migrations to revert.");
            return;
        }
    };

    let migration = migrations.iter().find(|m| m.version == last.0);
    match migration {
        None => {
            eprintln!(
                "error: applied migration {:04}_{} not found in '{}'",
                last.0,
                last.1,
                migrations_dir.display()
            );
            std::process::exit(1);
        }
        Some(m) => match &m.down_sql {
            None => {
                eprintln!(
                    "error: migration {:04}_{} has no DOWN section (add '-- +migrate Down' to the file)",
                    m.version, m.name
                );
                std::process::exit(1);
            }
            Some(down) => {
                print!("  Reverting {:04}_{} ...", m.version, m.name);
                if let Err(e) = exec_sql(db, database, down) {
                    println!(" FAILED");
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
                if let Err(e) = remove_applied(db, database, m.version) {
                    println!(" FAILED (removing state)");
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
                println!(" ok");
            }
        },
    }

    println!("Done.");
}

pub fn cmd_create(migrations_dir: &Path, name: &str) {
    if name.is_empty() {
        eprintln!("error: migration name must not be empty");
        std::process::exit(1);
    }
    // Sanitize name: replace spaces and special chars with underscores.
    let safe_name: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();

    if let Err(e) = std::fs::create_dir_all(migrations_dir) {
        eprintln!("error: cannot create migrations dir: {e}");
        std::process::exit(1);
    }

    // Find next version number.
    let next_version = match load_migrations(migrations_dir) {
        Ok(m) => m.last().map(|m| m.version + 1).unwrap_or(1),
        Err(_) => 1,
    };

    let filename = format!("{:04}_{safe_name}.sql", next_version);
    let path = migrations_dir.join(&filename);

    let template = format!(
        "-- Migration: {safe_name}\n\n-- Write your UP migration here:\n\n\n-- +migrate Down\n-- Write your DOWN migration here:\n\n"
    );

    if let Err(e) = std::fs::write(&path, template) {
        eprintln!("error: cannot write {}: {e}", path.display());
        std::process::exit(1);
    }

    println!("Created migration file: {}", path.display());
}

// ── CLI entry point ───────────────────────────────────────────────────────────

/// Parses and runs the `migrate` subcommand. Returns `true` if handled.
pub fn run_if_migrate(args: &[String]) -> bool {
    // args[0] is the binary name; look for "migrate" as the first real arg.
    let migrate_pos = args.iter().position(|a| a == "migrate");
    let Some(pos) = migrate_pos else {
        return false;
    };

    let rest = &args[pos + 1..];

    // Parse flags.
    let mut data_dir = PathBuf::from("./data");
    let mut database = "axiomdb".to_string();
    let mut migrations_dir = PathBuf::from("./migrations");
    let mut remaining: Vec<&str> = Vec::new();

    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--data-dir" if i + 1 < rest.len() => {
                data_dir = PathBuf::from(&rest[i + 1]);
                i += 2;
            }
            "--db" if i + 1 < rest.len() => {
                database = rest[i + 1].clone();
                i += 2;
            }
            "--dir" if i + 1 < rest.len() => {
                migrations_dir = PathBuf::from(&rest[i + 1]);
                i += 2;
            }
            other => {
                remaining.push(other);
                i += 1;
            }
        }
    }

    let subcommand = remaining.first().copied().unwrap_or("help");

    if subcommand == "create" {
        let name = remaining.get(1).copied().unwrap_or("");
        cmd_create(&migrations_dir, name);
        return true;
    }

    // All other commands require database access.
    let db = match axiomdb_network::mysql::SharedDatabase::open(&data_dir) {
        Ok(db) => Arc::new(db),
        Err(e) => {
            eprintln!(
                "error: cannot open database at '{}': {e}",
                data_dir.display()
            );
            std::process::exit(1);
        }
    };

    match subcommand {
        "status" => cmd_status(&db, &database, &migrations_dir),
        "up" => cmd_up(&db, &database, &migrations_dir),
        "down" => cmd_down(&db, &database, &migrations_dir),
        "help" | "--help" | "-h" => print_help(),
        other => {
            eprintln!("error: unknown migrate subcommand '{other}'");
            eprintln!("Run `axiomdb-server migrate help` for usage.");
            std::process::exit(1);
        }
    }

    true
}

fn print_help() {
    print!(concat!(
        "axiomdb-server migrate — Schema migration tool\n",
        "\n",
        "USAGE:\n",
        "    axiomdb-server migrate [OPTIONS] COMMAND\n",
        "\n",
        "COMMANDS:\n",
        "    status    Show migration status (applied / pending)\n",
        "    up        Apply all pending migrations\n",
        "    down      Revert the last applied migration\n",
        "    create    Create a new migration file\n",
        "    help      Show this help\n",
        "\n",
        "OPTIONS:\n",
        "    --data-dir PATH    AxiomDB data directory (default: ./data)\n",
        "    --db DATABASE      Target database name (default: axiomdb)\n",
        "    --dir PATH         Migrations directory (default: ./migrations)\n",
        "\n",
        "MIGRATION FILE FORMAT:\n",
        "    Files must be named `N_description.sql` (e.g., `001_create_users.sql`).\n",
        "\n",
        "    -- Write UP migration here\n",
        "    CREATE TABLE users (id INT PRIMARY KEY, name TEXT);\n",
        "\n",
        "    -- +migrate Down\n",
        "    DROP TABLE users;\n",
        "\n",
        "EXAMPLES:\n",
        "    axiomdb-server migrate status\n",
        "    axiomdb-server migrate up --dir db/migrations --db myapp\n",
        "    axiomdb-server migrate down\n",
        "    axiomdb-server migrate create add_users_table\n",
    ));
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn write_migration(dir: &Path, filename: &str, content: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(filename), content).unwrap();
    }

    fn open_test_db(dir: &Path) -> Arc<SharedDatabase> {
        Arc::new(SharedDatabase::open(dir).expect("open test db"))
    }

    // ── Migration file parsing ────────────────────────────────────────────────

    #[test]
    fn test_parse_version_and_name() {
        let tmp = tempfile::tempdir().unwrap();
        write_migration(
            tmp.path(),
            "001_create_users.sql",
            "CREATE TABLE users (id INT PRIMARY KEY);",
        );
        let migrations = load_migrations(tmp.path()).unwrap();
        assert_eq!(migrations.len(), 1);
        assert_eq!(migrations[0].version, 1);
        assert_eq!(migrations[0].name, "create_users");
        assert!(!migrations[0].up_sql.is_empty());
        assert!(migrations[0].down_sql.is_none());
    }

    #[test]
    fn test_parse_down_section() {
        let tmp = tempfile::tempdir().unwrap();
        write_migration(
            tmp.path(),
            "002_add_index.sql",
            "CREATE INDEX idx ON users (name);\n\n-- +migrate Down\nDROP INDEX idx ON users;",
        );
        let migrations = load_migrations(tmp.path()).unwrap();
        assert_eq!(migrations.len(), 1);
        assert_eq!(migrations[0].version, 2);
        let down = migrations[0].down_sql.as_deref().unwrap();
        assert!(down.contains("DROP INDEX"), "down_sql: {down}");
    }

    #[test]
    fn test_migrations_sorted_by_version() {
        let tmp = tempfile::tempdir().unwrap();
        write_migration(tmp.path(), "003_third.sql", "SELECT 3;");
        write_migration(tmp.path(), "001_first.sql", "SELECT 1;");
        write_migration(tmp.path(), "002_second.sql", "SELECT 2;");

        let migrations = load_migrations(tmp.path()).unwrap();
        assert_eq!(migrations.len(), 3);
        assert_eq!(migrations[0].version, 1);
        assert_eq!(migrations[1].version, 2);
        assert_eq!(migrations[2].version, 3);
    }

    #[test]
    fn test_duplicate_version_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        write_migration(tmp.path(), "001_first.sql", "SELECT 1;");
        write_migration(tmp.path(), "001_second.sql", "SELECT 2;");

        let err = load_migrations(tmp.path()).unwrap_err();
        assert!(err.contains("duplicate"), "expected duplicate error: {err}");
    }

    #[test]
    fn test_non_numeric_version_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        write_migration(tmp.path(), "abc_bad.sql", "SELECT 1;");

        let err = load_migrations(tmp.path()).unwrap_err();
        assert!(
            err.contains("number") || err.contains("version"),
            "expected version error: {err}"
        );
    }

    // ── up / status / down ────────────────────────────────────────────────────

    #[test]
    fn test_migrate_up_applies_all_pending() {
        let db_dir = tempfile::tempdir().unwrap();
        let mig_dir = tempfile::tempdir().unwrap();

        write_migration(
            mig_dir.path(),
            "001_orders.sql",
            "CREATE TABLE orders (id INT PRIMARY KEY, total INT);",
        );
        write_migration(
            mig_dir.path(),
            "002_status.sql",
            "ALTER TABLE orders ADD COLUMN status TEXT;",
        );

        let db = open_test_db(db_dir.path());
        cmd_up(&db, "axiomdb", mig_dir.path());

        let applied = applied_versions(&db, "axiomdb").unwrap();
        assert_eq!(applied.len(), 2);
        assert_eq!(applied[0].0, 1);
        assert_eq!(applied[1].0, 2);
    }

    #[test]
    fn test_migrate_up_is_idempotent() {
        let db_dir = tempfile::tempdir().unwrap();
        let mig_dir = tempfile::tempdir().unwrap();

        write_migration(
            mig_dir.path(),
            "001_items.sql",
            "CREATE TABLE items (id INT PRIMARY KEY);",
        );

        let db = open_test_db(db_dir.path());
        cmd_up(&db, "axiomdb", mig_dir.path());
        cmd_up(&db, "axiomdb", mig_dir.path());

        let applied = applied_versions(&db, "axiomdb").unwrap();
        assert_eq!(applied.len(), 1, "should still have exactly 1 applied");
    }

    #[test]
    fn test_migrate_down_reverts_last() {
        let db_dir = tempfile::tempdir().unwrap();
        let mig_dir = tempfile::tempdir().unwrap();

        write_migration(
            mig_dir.path(),
            "001_tbl.sql",
            "CREATE TABLE tbl (id INT PRIMARY KEY);\n\n-- +migrate Down\nDROP TABLE tbl;",
        );

        let db = open_test_db(db_dir.path());
        cmd_up(&db, "axiomdb", mig_dir.path());
        assert_eq!(applied_versions(&db, "axiomdb").unwrap().len(), 1);

        cmd_down(&db, "axiomdb", mig_dir.path());
        assert_eq!(
            applied_versions(&db, "axiomdb").unwrap().len(),
            0,
            "migration should be reverted"
        );
    }

    #[test]
    fn test_migrate_create_generates_file() {
        let mig_dir = tempfile::tempdir().unwrap();
        cmd_create(mig_dir.path(), "add users table");

        let files: Vec<_> = std::fs::read_dir(mig_dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(files.len(), 1);
        let name = files[0].file_name();
        let name = name.to_string_lossy();
        assert!(name.starts_with("0001_"), "unexpected: {name}");
        assert!(name.ends_with(".sql"), "must be .sql: {name}");

        let content = std::fs::read_to_string(files[0].path()).unwrap();
        assert!(
            content.contains("+migrate Down"),
            "template must include DOWN marker"
        );
    }

    #[test]
    fn test_migrate_create_increments_version() {
        let mig_dir = tempfile::tempdir().unwrap();
        write_migration(mig_dir.path(), "001_first.sql", "SELECT 1;");
        write_migration(mig_dir.path(), "002_second.sql", "SELECT 2;");

        cmd_create(mig_dir.path(), "third");

        let migrations = load_migrations(mig_dir.path()).unwrap();
        assert_eq!(migrations.len(), 3);
        assert_eq!(migrations[2].version, 3);
    }

    #[test]
    fn test_split_statements_handles_semicolons_in_strings() {
        let sql = "INSERT INTO t VALUES ('a;b');";
        let stmts = split_statements(sql);
        assert_eq!(
            stmts.len(),
            1,
            "semicolon inside string should not split: {stmts:?}"
        );
    }

    #[test]
    fn test_split_statements_multiple() {
        let sql = "CREATE TABLE a (id INT);\nCREATE TABLE b (id INT);";
        let stmts: Vec<_> = split_statements(sql)
            .into_iter()
            .filter(|s| !s.trim().is_empty())
            .collect();
        assert_eq!(stmts.len(), 2);
    }
}
