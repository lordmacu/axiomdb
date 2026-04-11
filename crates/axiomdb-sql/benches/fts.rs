//! Benchmarks for Full-Text Search — parsing and evaluation.

use axiomdb_sql::fts_query::{evaluate_fts, parse_fts_query};
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

// ── Fixtures ──────────────────────────────────────────────────────────────────

const SHORT_DOC: &str = "the quick brown fox jumps over the lazy dog";
const MEDIUM_DOC: &str = "AxiomDB is a high-performance database engine written in Rust. \
It features a custom SQL parser, B+ tree indexes, WAL-based crash recovery, \
MVCC transaction isolation, and a vectorized query executor. The storage \
engine uses memory-mapped files with a buffer pool manager for optimal I/O. \
Full-text search is supported through an inverted index with BM25 ranking.";
const LONG_DOC: &str = "Database systems have evolved significantly over the past decades. \
Modern database engines combine multiple storage paradigms including B-tree indexes, \
LSM-tree compaction, and columnar storage formats. Transaction processing requires \
careful handling of concurrency through mechanisms like MVCC, optimistic concurrency \
control, and two-phase locking. Query optimization involves cost-based planning with \
statistics, join reordering, predicate pushdown, and index selection. The recovery \
subsystem ensures durability through write-ahead logging with checkpoints and \
incremental snapshots. Distributed database systems add complexity with consensus \
protocols like Raft and Paxos, distributed transactions, and data partitioning \
strategies including hash and range sharding across multiple nodes.";

// ── Parse benchmarks ──────────────────────────────────────────────────────────

fn bench_parse(c: &mut Criterion) {
    let mut group = c.benchmark_group("fts/parse");

    for (name, query) in [
        ("single_term", "database"),
        ("required", "+database +engine"),
        ("boolean_mixed", "+database -mysql engine | storage"),
        ("phrase", "\"write ahead log\""),
        ("prefix", "data*"),
        (
            "complex",
            "+database -mysql \"crash recovery\" index* | btree",
        ),
    ] {
        group.throughput(Throughput::Elements(1));
        group.bench_function(name, |b| b.iter(|| parse_fts_query(black_box(query))));
    }

    group.finish();
}

// ── Evaluate benchmarks ──────────────────────────────────────────────────────

fn bench_evaluate(c: &mut Criterion) {
    let mut group = c.benchmark_group("fts/evaluate");

    for (doc_name, doc) in [
        ("short", SHORT_DOC),
        ("medium", MEDIUM_DOC),
        ("long", LONG_DOC),
    ] {
        // Single term
        let clauses = parse_fts_query("database");
        group.bench_with_input(
            BenchmarkId::new("single_term", doc_name),
            &(clauses, doc),
            |b, (clauses, doc)| b.iter(|| evaluate_fts(black_box(clauses), black_box(doc))),
        );

        // Boolean query
        let clauses = parse_fts_query("+database +engine -mysql");
        group.bench_with_input(
            BenchmarkId::new("boolean", doc_name),
            &(clauses, doc),
            |b, (clauses, doc)| b.iter(|| evaluate_fts(black_box(clauses), black_box(doc))),
        );

        // Phrase query
        let clauses = parse_fts_query("\"crash recovery\"");
        group.bench_with_input(
            BenchmarkId::new("phrase", doc_name),
            &(clauses, doc),
            |b, (clauses, doc)| b.iter(|| evaluate_fts(black_box(clauses), black_box(doc))),
        );

        // Prefix query
        let clauses = parse_fts_query("data*");
        group.bench_with_input(
            BenchmarkId::new("prefix", doc_name),
            &(clauses, doc),
            |b, (clauses, doc)| b.iter(|| evaluate_fts(black_box(clauses), black_box(doc))),
        );
    }

    // Bulk: evaluate N documents
    let clauses = parse_fts_query("+database engine");
    let docs: Vec<String> = (0..1000)
        .map(|i| format!("document {i} about database engine storage index query"))
        .collect();
    group.throughput(Throughput::Elements(1000));
    group.bench_function("bulk_1000_docs", |b| {
        b.iter(|| {
            for doc in &docs {
                black_box(evaluate_fts(&clauses, doc));
            }
        })
    });

    group.finish();
}

criterion_group!(benches, bench_parse, bench_evaluate);
criterion_main!(benches);
