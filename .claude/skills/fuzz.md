# /fuzz — Testing con entradas aleatorias

Crítico para el parser SQL (entradas malformadas) y el storage (páginas corruptas).

## Setup inicial (una vez)

```bash
# Instalar cargo-fuzz
cargo install cargo-fuzz

# Crear targets de fuzz
cd /Users/cristian/dbyo
cargo fuzz init   # si es la primera vez

# Targets para dbyo
cargo fuzz add fuzz_sql_parser      # parsear SQL aleatorio
cargo fuzz add fuzz_storage_pages   # páginas con bytes corruptos
cargo fuzz add fuzz_wal_recovery    # WAL truncado o corrupto
cargo fuzz add fuzz_btree_ops       # operaciones en el B+ Tree
```

## Implementar cada target

```rust
// fuzz/fuzz_targets/fuzz_sql_parser.rs
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(sql) = std::str::from_utf8(data) {
        // El parser NUNCA debe paniquear con entrada arbitraria
        // Solo puede retornar Ok o Err — nunca panic/crash
        let _ = dbyo_sql::Parser::new().parse(sql);
    }
});

// fuzz/fuzz_targets/fuzz_storage_pages.rs
fuzz_target!(|data: &[u8]| {
    if data.len() < 8192 { return; }
    let mut page_bytes = [0u8; 8192];
    page_bytes.copy_from_slice(&data[..8192]);

    // El storage NUNCA debe paniquear leyendo página corrupta
    let _ = dbyo_storage::Page::from_bytes(&page_bytes);
});
```

## Correr fuzz tests

```bash
# Correr un target (Ctrl+C para parar)
cargo fuzz run fuzz_sql_parser

# Con timeout
cargo fuzz run fuzz_sql_parser -- -max_total_time=300   # 5 minutos

# Ver cobertura de código
cargo fuzz coverage fuzz_sql_parser
cargo fuzz fmt coverage fuzz_sql_parser

# Ver crashes encontrados
ls fuzz/artifacts/fuzz_sql_parser/
```

## Por cada crash encontrado

```bash
# Reproducir el crash
cargo fuzz run fuzz_sql_parser fuzz/artifacts/fuzz_sql_parser/crash-HASH

# Minimizar el input (encontrar el mínimo que reproduce el crash)
cargo fuzz tmin fuzz_sql_parser fuzz/artifacts/fuzz_sql_parser/crash-HASH
```

```rust
// Agregar como test de regresión permanente
#[test]
fn test_fuzz_regression_sql_crash_20260321() {
    // Input que causó crash en fuzz testing (2026-03-21)
    // Causa: el parser no manejaba UTF-8 inválido en nombre de tabla
    let input = b"\xff\xfe SELECT * FROM";
    let result = Parser::new().parse(std::str::from_utf8(input).unwrap_or(""));
    // Debe retornar Err, nunca panic
    assert!(result.is_err() || result.is_ok()); // no debe llegar aquí si hubo panic
}
```

## En CI (automatizar)

```yaml
# .github/workflows/fuzz.yml
- name: Fuzz SQL Parser (60s)
  run: cargo fuzz run fuzz_sql_parser -- -max_total_time=60

- name: Fuzz Storage (60s)
  run: cargo fuzz run fuzz_storage_pages -- -max_total_time=60
```
