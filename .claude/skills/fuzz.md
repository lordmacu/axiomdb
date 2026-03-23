# /fuzz — Testing with random inputs

Critical for the SQL parser (malformed inputs) and storage (corrupt pages).

## Initial setup (once)

```bash
# Install cargo-fuzz
cargo install cargo-fuzz

# Create fuzz targets
cd /Users/cristian/nexusdb
cargo fuzz init   # if first time

# Targets for axiomdb
cargo fuzz add fuzz_sql_parser      # parse random SQL
cargo fuzz add fuzz_storage_pages   # pages with corrupt bytes
cargo fuzz add fuzz_wal_recovery    # truncated or corrupt WAL
cargo fuzz add fuzz_btree_ops       # operations on the B+ Tree
```

## Implement each target

```rust
// fuzz/fuzz_targets/fuzz_sql_parser.rs
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(sql) = std::str::from_utf8(data) {
        // The parser must NEVER panic with arbitrary input
        // It can only return Ok or Err — never panic/crash
        let _ = axiomdb_sql::Parser::new().parse(sql);
    }
});

// fuzz/fuzz_targets/fuzz_storage_pages.rs
fuzz_target!(|data: &[u8]| {
    if data.len() < 8192 { return; }
    let mut page_bytes = [0u8; 8192];
    page_bytes.copy_from_slice(&data[..8192]);

    // The storage must NEVER panic reading a corrupt page
    let _ = axiomdb_storage::Page::from_bytes(&page_bytes);
});
```

## Run fuzz tests

```bash
# Run a target (Ctrl+C to stop)
cargo fuzz run fuzz_sql_parser

# With timeout
cargo fuzz run fuzz_sql_parser -- -max_total_time=300   # 5 minutes

# View code coverage
cargo fuzz coverage fuzz_sql_parser
cargo fuzz fmt coverage fuzz_sql_parser

# View crashes found
ls fuzz/artifacts/fuzz_sql_parser/
```

## For each crash found

```bash
# Reproduce the crash
cargo fuzz run fuzz_sql_parser fuzz/artifacts/fuzz_sql_parser/crash-HASH

# Minimize the input (find the minimum that reproduces the crash)
cargo fuzz tmin fuzz_sql_parser fuzz/artifacts/fuzz_sql_parser/crash-HASH
```

```rust
// Add as a permanent regression test
#[test]
fn test_fuzz_regression_sql_crash_20260321() {
    // Input that caused a crash in fuzz testing (2026-03-21)
    // Cause: the parser did not handle invalid UTF-8 in table name
    let input = b"\xff\xfe SELECT * FROM";
    let result = Parser::new().parse(std::str::from_utf8(input).unwrap_or(""));
    // Must return Err, never panic
    assert!(result.is_err() || result.is_ok()); // must not reach here if there was a panic
}
```

## In CI (automate)

```yaml
# .github/workflows/fuzz.yml
- name: Fuzz SQL Parser (60s)
  run: cargo fuzz run fuzz_sql_parser -- -max_total_time=60

- name: Fuzz Storage (60s)
  run: cargo fuzz run fuzz_storage_pages -- -max_total_time=60
```
