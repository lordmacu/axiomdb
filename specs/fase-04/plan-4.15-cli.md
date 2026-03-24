# Plan: 4.15 — Interactive CLI (REPL)

## Files to create/modify

| Acción | Archivo | Qué hace |
|---|---|---|
| **Create** | `crates/axiomdb-cli/Cargo.toml` | New crate, binary, dependencies |
| **Create** | `crates/axiomdb-cli/src/main.rs` | Entry point, arg parsing, DB open |
| **Create** | `crates/axiomdb-cli/src/repl.rs` | REPL loop, multi-line accumulator |
| **Create** | `crates/axiomdb-cli/src/table.rs` | ASCII table formatter |
| **Create** | `crates/axiomdb-cli/src/dot.rs` | Dot command handlers |
| Modify | `Cargo.toml` (workspace root) | Add `axiomdb-cli` to `members` |

---

## Algorithm — REPL loop

```rust
// repl.rs
pub fn run(db: &mut Database) {
    let is_tty = std::io::stdin().is_terminal();
    let stdin = std::io::stdin();
    let mut buffer = String::new(); // accumulates multi-line input

    loop {
        if is_tty {
            let prompt = if buffer.trim().is_empty() { "axiomdb> " } else { "   -> " };
            print!("{prompt}");
            std::io::stdout().flush().ok();
        }

        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => break,              // EOF (Ctrl-D)
            Ok(_) => {}
            Err(_) => break,
        }

        let trimmed = line.trim_end();

        // Dot commands — only when buffer is empty
        if buffer.trim().is_empty() {
            let lower = trimmed.trim().to_ascii_lowercase();
            if lower.starts_with('.') || lower == "\\q" {
                match handle_dot_command(trimmed.trim(), db) {
                    DotResult::Quit => break,
                    DotResult::Ok => continue,
                    DotResult::Error(e) => {
                        eprintln!("Error: {e}");
                        continue;
                    }
                }
            }
        }

        buffer.push_str(trimmed);
        buffer.push('\n');

        // Execute when the trimmed line ends with ';'
        if trimmed.ends_with(';') {
            let sql = std::mem::take(&mut buffer);
            let t0 = std::time::Instant::now();

            let mut schema_cache = SchemaCache::new();
            let mut session = SessionContext::new();

            match db.execute_query(sql.trim(), &mut session, &mut schema_cache) {
                Ok((result, _commit_rx)) => {
                    let elapsed = t0.elapsed();
                    print_result(result, elapsed);
                }
                Err(e) => {
                    eprintln!("Error [{}]: {}", e.sqlstate(), e);
                }
            }
        }
    }

    if is_tty { println!("Bye."); }
}
```

**Note:** `_commit_rx` is ignored — CLI commits are always sync (group commit disabled).

---

## Algorithm — ASCII table formatter

```rust
// table.rs
pub fn format_rows(cols: &[ColumnMeta], rows: &[Row]) -> String {
    // 1. Compute column widths = max(header_len, max_value_len)
    let mut widths: Vec<usize> = cols.iter().map(|c| c.name.len()).collect();
    let rendered: Vec<Vec<String>> = rows.iter()
        .map(|row| row.iter().zip(cols.iter())
            .map(|(v, c)| render_value(v, c))
            .collect())
        .collect();
    for row in &rendered {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }

    let mut out = String::new();
    let sep = make_separator(&widths);

    out.push_str(&sep);
    out.push_str(&make_header(cols, &widths));
    out.push_str(&sep);
    for row in &rendered {
        out.push_str(&make_row(row, cols, &widths));
    }
    out.push_str(&sep);
    out
}

fn render_value(v: &Value, col: &ColumnMeta) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        Value::Uuid(bytes) => format_uuid(bytes),
        Value::Bytes(b) => format!("<{} bytes>", b.len()),
        Value::Text(s) if s.len() > 64 => format!("{}…", &s[..63]),
        _ => format!("{v}"),
    }
}

fn is_numeric(col: &ColumnMeta) -> bool {
    matches!(col.data_type, DataType::Int | DataType::BigInt | DataType::Real | DataType::Double)
}
// Numeric values are right-aligned; all others left-aligned.
```

---

## Algorithm — Dot commands

```rust
// dot.rs
pub enum DotResult { Quit, Ok, Error(String) }

pub fn handle_dot_command(cmd: &str, db: &mut Database) -> DotResult {
    let parts: Vec<&str> = cmd.splitn(2, ' ').collect();
    match parts[0].to_ascii_lowercase().as_str() {
        ".quit" | ".exit" | "\\q" => DotResult::Quit,

        ".help" => {
            println!("  .help               Show this message");
            println!("  .tables             List all user tables");
            println!("  .schema [table]     Show columns and indexes");
            println!("  .open <path>        Open a different database file");
            println!("  .quit               Exit (also: .exit, \\q, Ctrl-D)");
            DotResult::Ok
        }

        ".tables" => {
            // Execute SHOW TABLES and print one name per line
            let mut session = SessionContext::new();
            let mut cache = SchemaCache::new();
            match db.execute_query("SHOW TABLES", &mut session, &mut cache) {
                Ok((QueryResult::Rows { rows, .. }, _)) => {
                    let mut names: Vec<String> = rows.iter()
                        .filter_map(|r| r.first())
                        .filter_map(|v| if let Value::Text(s) = v { Some(s.clone()) } else { None })
                        .collect();
                    names.sort();
                    for name in names { println!("{name}"); }
                    DotResult::Ok
                }
                Ok(_) => DotResult::Ok,
                Err(e) => DotResult::Error(e.to_string()),
            }
        }

        ".schema" => {
            let table_filter = parts.get(1).map(|s| s.trim());
            print_schema(db, table_filter)
        }

        ".open" => {
            // Close current DB and open a new one
            let path = match parts.get(1) {
                Some(p) => p.trim(),
                None => return DotResult::Error(".open requires a path".into()),
            };
            match Database::open(std::path::Path::new(path)) {
                Ok(new_db) => { *db = new_db; DotResult::Ok }
                Err(e) => DotResult::Error(format!("failed to open '{path}': {e}")),
            }
        }

        other => DotResult::Error(format!("unknown command '{other}' — type .help")),
    }
}
```

---

## main.rs

```rust
fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| "./axiomdb.db".into());
    let data_path = std::path::Path::new(&path);

    let mut db = match Database::open(data_path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("axiomdb-cli: failed to open '{}': {}", path, e);
            std::process::exit(1);
        }
    };

    if std::io::stdin().is_terminal() {
        println!("AxiomDB {} — interactive shell", env!("CARGO_PKG_VERSION"));
        println!("Type SQL ending with ; to execute. Type .help for commands.");
        println!();
    }

    repl::run(&mut db);
}
```

---

## Fases de implementación

### Fase 1 — Scaffolding (10 min)
1. Crear `crates/axiomdb-cli/Cargo.toml` con deps
2. Añadir `axiomdb-cli` a workspace `members` en root `Cargo.toml`
3. `main.rs` stub que abre DB y sale
4. `cargo build -p axiomdb-cli` debe compilar

### Fase 2 — Table formatter (20 min)
1. `table.rs` completo con `format_rows()`
2. `render_value()` para todos los tipos
3. Unit test: `test_format_empty`, `test_format_two_rows`, `test_null_display`, `test_bytes_display`

### Fase 3 — Dot commands (15 min)
1. `dot.rs` con todos los comandos del spec
2. `print_schema()` — usa SHOW COLUMNS (o resolve directo)

### Fase 4 — REPL loop (20 min)
1. `repl.rs` con acumulación multi-línea, TTY detection, timing
2. Integración en `main.rs`
3. Test manual: multi-línea, EOF, errores

### Fase 5 — Integration test (10 min)
- Script SQL pipe: `echo "SELECT 1;" | cargo run -p axiomdb-cli -- /tmp/test.db`

---

## Anti-patterns a evitar

- **NO usar `tokio` en el CLI** — todo síncrono, `Database::execute_query` ya es síncrono
- **NO imprimir prompt si stdin no es TTY** — comprobar con `std::io::IsTerminal`
- **NO hacer panic en valores inesperados** — el formatter debe manejar cualquier `Value` gracefully
- **NO usar `println!` para el prompt** — usar `print!` + `flush()` para que quede en la misma línea

---

## Cargo.toml del nuevo crate

```toml
[package]
name        = "axiomdb-cli"
version.workspace = true
edition.workspace = true
authors.workspace = true
license.workspace = true

[[bin]]
name = "axiomdb-cli"
path = "src/main.rs"

[dependencies]
axiomdb-core    = { workspace = true }
axiomdb-network = { path = "../axiomdb-network" }
axiomdb-sql     = { path = "../axiomdb-sql" }
axiomdb-types   = { path = "../axiomdb-types" }
```
