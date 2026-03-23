//! Benchmarks for SQL components: lexer, parser, expression evaluator.

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use nexusdb_sql::{
    eval, is_truthy,
    expr::{BinaryOp, Expr, UnaryOp},
    parse, tokenize,
};
use nexusdb_types::Value;

// ── SQL fixtures ──────────────────────────────────────────────────────────────

const SIMPLE_SELECT: &str = "SELECT id, name FROM users WHERE id = 1";
const COMPLEX_SELECT: &str = "
    SELECT DISTINCT u.id AS user_id, u.name, u.email, COUNT(o.id) AS orders
    FROM users AS u
    LEFT JOIN orders AS o ON u.id = o.user_id
    WHERE u.active = TRUE AND u.age >= 18 AND u.email IS NOT NULL
    GROUP BY u.id, u.name, u.email
    HAVING COUNT(o.id) > 0
    ORDER BY orders DESC NULLS LAST, u.name ASC
    LIMIT 50 OFFSET 0
";
const CREATE_TABLE: &str = "
    CREATE TABLE IF NOT EXISTS products (
        id          BIGINT PRIMARY KEY AUTO_INCREMENT,
        name        TEXT NOT NULL,
        price       REAL DEFAULT 0.0,
        stock       INT DEFAULT 0,
        category_id BIGINT REFERENCES categories(id) ON DELETE SET NULL,
        active      BOOL DEFAULT TRUE,
        created_at  TIMESTAMP
    )
";
const INSERT_STMT: &str =
    "INSERT INTO orders (user_id, product_id, qty, total) VALUES (1, 2, 3, 99.99)";
const UPDATE_STMT: &str = "UPDATE products SET price = 9.99, stock = stock - 1 WHERE id = 42";
const DELETE_STMT: &str = "DELETE FROM sessions WHERE expires_at < 1704067200";

// ── Lexer benchmarks ──────────────────────────────────────────────────────────

fn bench_lexer(c: &mut Criterion) {
    let mut group = c.benchmark_group("lexer");

    for (name, sql) in [
        ("simple_select", SIMPLE_SELECT),
        ("complex_select", COMPLEX_SELECT),
        ("create_table", CREATE_TABLE),
        ("insert", INSERT_STMT),
    ] {
        group.throughput(Throughput::Bytes(sql.len() as u64));
        group.bench_function(name, |b| {
            b.iter(|| tokenize(black_box(sql), None).unwrap())
        });
    }

    group.finish();
}

// ── Parser benchmarks ─────────────────────────────────────────────────────────

fn bench_parser(c: &mut Criterion) {
    let mut group = c.benchmark_group("parser");

    for (name, sql) in [
        ("simple_select", SIMPLE_SELECT),
        ("complex_select", COMPLEX_SELECT),
        ("create_table", CREATE_TABLE),
        ("insert", INSERT_STMT),
        ("update", UPDATE_STMT),
        ("delete", DELETE_STMT),
    ] {
        group.throughput(Throughput::Elements(1));
        group.bench_function(name, |b| {
            b.iter(|| parse(black_box(sql), None).unwrap())
        });
    }

    group.finish();
}

// ── Lexer + parse throughput (queries/second) ─────────────────────────────────

fn bench_qps(c: &mut Criterion) {
    let mut group = c.benchmark_group("qps");

    for n_queries in [100u64, 1_000] {
        group.throughput(Throughput::Elements(n_queries));

        group.bench_with_input(
            BenchmarkId::new("simple_select", n_queries),
            &n_queries,
            |b, &n| {
                b.iter(|| {
                    for _ in 0..n {
                        black_box(parse(SIMPLE_SELECT, None).unwrap());
                    }
                })
            },
        );
    }

    group.finish();
}

// ── Expression evaluator benchmarks ──────────────────────────────────────────

fn bench_eval(c: &mut Criterion) {
    let mut group = c.benchmark_group("eval");

    // Row context: [id=1, age=25, email="alice@x.com", active=true]
    let row = vec![
        Value::BigInt(1),
        Value::Int(25),
        Value::Text("alice@example.com".into()),
        Value::Bool(true),
        Value::Null, // email_verified = NULL
    ];

    // Benchmark 1: simple equality  `id = 1`
    let eq_expr = Expr::BinaryOp {
        op: BinaryOp::Eq,
        left: Box::new(Expr::Column { col_idx: 0, name: "id".into() }),
        right: Box::new(Expr::Literal(Value::BigInt(1))),
    };
    group.bench_function("eq_col_literal", |b| {
        b.iter(|| {
            let v = eval(black_box(&eq_expr), black_box(&row)).unwrap();
            black_box(is_truthy(&v))
        })
    });

    // Benchmark 2: AND with IS NOT NULL  `age > 18 AND email IS NOT NULL`
    let and_expr = Expr::BinaryOp {
        op: BinaryOp::And,
        left: Box::new(Expr::BinaryOp {
            op: BinaryOp::Gt,
            left: Box::new(Expr::Column { col_idx: 1, name: "age".into() }),
            right: Box::new(Expr::Literal(Value::Int(18))),
        }),
        right: Box::new(Expr::IsNull {
            expr: Box::new(Expr::Column { col_idx: 4, name: "email_verified".into() }),
            negated: true,
        }),
    };
    group.bench_function("and_gt_isnull", |b| {
        b.iter(|| {
            let v = eval(black_box(&and_expr), black_box(&row)).unwrap();
            black_box(is_truthy(&v))
        })
    });

    // Benchmark 3: BETWEEN  `age BETWEEN 18 AND 65`
    let between_expr = Expr::Between {
        expr: Box::new(Expr::Column { col_idx: 1, name: "age".into() }),
        low: Box::new(Expr::Literal(Value::Int(18))),
        high: Box::new(Expr::Literal(Value::Int(65))),
        negated: false,
    };
    group.bench_function("between", |b| {
        b.iter(|| {
            let v = eval(black_box(&between_expr), black_box(&row)).unwrap();
            black_box(is_truthy(&v))
        })
    });

    // Benchmark 4: complex nested  `(id > 0 AND age >= 18) OR active = TRUE`
    let complex_expr = Expr::BinaryOp {
        op: BinaryOp::Or,
        left: Box::new(Expr::BinaryOp {
            op: BinaryOp::And,
            left: Box::new(Expr::BinaryOp {
                op: BinaryOp::Gt,
                left: Box::new(Expr::Column { col_idx: 0, name: "id".into() }),
                right: Box::new(Expr::Literal(Value::Int(0))),
            }),
            right: Box::new(Expr::BinaryOp {
                op: BinaryOp::GtEq,
                left: Box::new(Expr::Column { col_idx: 1, name: "age".into() }),
                right: Box::new(Expr::Literal(Value::Int(18))),
            }),
        }),
        right: Box::new(Expr::BinaryOp {
            op: BinaryOp::Eq,
            left: Box::new(Expr::Column { col_idx: 3, name: "active".into() }),
            right: Box::new(Expr::Literal(Value::Bool(true))),
        }),
    };
    group.bench_function("complex_or_and", |b| {
        b.iter(|| {
            let v = eval(black_box(&complex_expr), black_box(&row)).unwrap();
            black_box(is_truthy(&v))
        })
    });

    // Benchmark 5: NULL propagation  `NULL AND TRUE`
    let null_expr = Expr::BinaryOp {
        op: BinaryOp::And,
        left: Box::new(Expr::Literal(Value::Null)),
        right: Box::new(Expr::Literal(Value::Bool(true))),
    };
    group.bench_function("null_and_true", |b| {
        b.iter(|| {
            let v = eval(black_box(&null_expr), black_box(&row)).unwrap();
            black_box(is_truthy(&v))
        })
    });

    // Benchmark 6: eval 1000 rows with a WHERE predicate
    let predicate = Expr::BinaryOp {
        op: BinaryOp::And,
        left: Box::new(Expr::BinaryOp {
            op: BinaryOp::Gt,
            left: Box::new(Expr::Column { col_idx: 1, name: "age".into() }),
            right: Box::new(Expr::Literal(Value::Int(18))),
        }),
        right: Box::new(Expr::BinaryOp {
            op: BinaryOp::Eq,
            left: Box::new(Expr::Column { col_idx: 3, name: "active".into() }),
            right: Box::new(Expr::Literal(Value::Bool(true))),
        }),
    };
    let rows: Vec<Vec<Value>> = (0..1000)
        .map(|i| {
            vec![
                Value::BigInt(i),
                Value::Int((i % 80) as i32),
                Value::Text("x@x.com".into()),
                Value::Bool(i % 2 == 0),
                Value::Null,
            ]
        })
        .collect();
    group.throughput(Throughput::Elements(1000));
    group.bench_function("scan_1000_rows", |b| {
        b.iter(|| {
            let mut count = 0usize;
            for row in &rows {
                let v = eval(black_box(&predicate), black_box(row)).unwrap();
                if is_truthy(&v) {
                    count += 1;
                }
            }
            black_box(count)
        })
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_lexer,
    bench_parser,
    bench_qps,
    bench_eval,
);
criterion_main!(benches);
