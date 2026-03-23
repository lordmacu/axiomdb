//! Parser comparison benchmark — NexusDB vs sqlparser-rs.
//!
//! sqlparser-rs is the most popular production SQL parser in Rust,
//! used by Apache Arrow DataFusion, Delta Lake, Ballista, and others.
//! It supports MySQL, PostgreSQL, Snowflake, and 20+ dialects.
//!
//! ## What this measures
//!
//! Pure parsing throughput (no execution, no network, no I/O).
//! Both parsers receive the same SQL string and return an AST.
//! This is the fairest possible apples-to-apples comparison.
//!
//! ## MySQL and PostgreSQL context
//!
//! Real MySQL/PostgreSQL benchmarks measure full round-trips
//! (network + parse + plan + execute + serialize + send).
//! Even `SELECT 1` over localhost takes 100-500µs in those systems.
//! Our parser-only numbers are therefore much faster than full-stack
//! comparisons — they show the overhead budget available for execution.

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use sqlparser::{dialect::MySqlDialect, dialect::PostgreSqlDialect, parser::Parser as SpParser};

// ── SQL fixtures (same as sql_components bench) ───────────────────────────────

const SIMPLE_SELECT: &str = "SELECT id, name FROM users WHERE id = 1";

const MEDIUM_SELECT: &str = "SELECT u.id, u.name, COUNT(o.id) AS orders \
     FROM users AS u \
     LEFT JOIN orders AS o ON u.id = o.user_id \
     WHERE u.active = TRUE AND u.age >= 18 \
     GROUP BY u.id, u.name \
     ORDER BY orders DESC \
     LIMIT 50";

const COMPLEX_SELECT: &str = "
    SELECT DISTINCT u.id AS user_id, u.name, u.email, COUNT(o.id) AS orders
    FROM users AS u
    LEFT JOIN orders AS o ON u.id = o.user_id
    WHERE u.active = TRUE AND u.age >= 18 AND u.email IS NOT NULL
    GROUP BY u.id, u.name, u.email
    HAVING COUNT(o.id) > 0
    ORDER BY orders DESC, u.name ASC
    LIMIT 50 OFFSET 0
";

const CREATE_TABLE: &str = "
    CREATE TABLE IF NOT EXISTS products (
        id          BIGINT PRIMARY KEY AUTO_INCREMENT,
        name        TEXT NOT NULL,
        price       REAL DEFAULT 0.0,
        stock       INT DEFAULT 0,
        active      BOOL DEFAULT TRUE,
        created_at  TIMESTAMP
    )
";

const INSERT_STMT: &str =
    "INSERT INTO orders (user_id, product_id, qty, total) VALUES (1, 2, 3, 99.99)";

const UPDATE_STMT: &str = "UPDATE products SET price = 9.99, stock = stock - 1 WHERE id = 42";

// ── Benchmark ─────────────────────────────────────────────────────────────────

fn bench_comparison(c: &mut Criterion) {
    let mysql_dialect = MySqlDialect {};
    let pg_dialect = PostgreSqlDialect {};

    for (name, sql) in [
        ("simple_select", SIMPLE_SELECT),
        ("medium_select", MEDIUM_SELECT),
        ("complex_select", COMPLEX_SELECT),
        ("create_table", CREATE_TABLE),
        ("insert", INSERT_STMT),
        ("update", UPDATE_STMT),
    ] {
        let mut group = c.benchmark_group(format!("compare/{name}"));
        group.throughput(Throughput::Elements(1));

        // NexusDB parser
        group.bench_function("nexusdb", |b| {
            b.iter(|| nexusdb_sql::parse(black_box(sql), None).unwrap())
        });

        // sqlparser-rs with MySQL dialect
        group.bench_function("sqlparser_mysql", |b| {
            b.iter(|| SpParser::parse_sql(&mysql_dialect, black_box(sql)).unwrap())
        });

        // sqlparser-rs with PostgreSQL dialect
        group.bench_function("sqlparser_pg", |b| {
            b.iter(|| SpParser::parse_sql(&pg_dialect, black_box(sql)).unwrap())
        });

        group.finish();
    }
}

// ── Throughput at scale ───────────────────────────────────────────────────────

fn bench_throughput(c: &mut Criterion) {
    let mysql_dialect = MySqlDialect {};

    for n_queries in [1_000u64, 10_000] {
        let mut group = c.benchmark_group("throughput");
        group.throughput(Throughput::Elements(n_queries));

        group.bench_with_input(
            BenchmarkId::new("nexusdb_simple_select", n_queries),
            &n_queries,
            |b, &n| {
                b.iter(|| {
                    for _ in 0..n {
                        black_box(nexusdb_sql::parse(SIMPLE_SELECT, None).unwrap());
                    }
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("sqlparser_simple_select", n_queries),
            &n_queries,
            |b, &n| {
                b.iter(|| {
                    for _ in 0..n {
                        black_box(SpParser::parse_sql(&mysql_dialect, SIMPLE_SELECT).unwrap());
                    }
                })
            },
        );

        group.finish();
    }
}

criterion_group!(benches, bench_comparison, bench_throughput);
criterion_main!(benches);
