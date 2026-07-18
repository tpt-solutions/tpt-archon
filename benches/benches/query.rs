//! Query-engine micro-benchmarks for `tpt-archon-relational`.
//!
//! Measures the parse -> plan -> execute pipeline and the CPU vector-search
//! fallback, so the `spec.txt` vector-search and query metrics can be tracked
//! against real numbers (GPU and pgvector comparisons plug in here later).

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use tpt_archon_relational::executor::{execute, vector_topk, Table, Value};
use tpt_archon_relational::parser::parse_select;
use tpt_archon_relational::planner::{plan_select, TableStats};

fn make_table(n: i64) -> Table {
    let mut t = Table::new(vec!["id".to_string(), "age".to_string()]);
    for i in 0..n {
        t.insert(vec![Value::Int(i), Value::Int(i % 100)]);
    }
    t
}

fn bench_query(c: &mut Criterion) {
    let table = make_table(100_000);
    let sql = "SELECT id FROM t WHERE age >= 50";
    let stmt = parse_select(sql).unwrap();
    let plan = plan_select(
        &stmt,
        TableStats {
            row_count: table.rows.len() as u64,
        },
    );

    c.bench_function("select_filter_project_100k", |b| {
        b.iter(|| black_box(execute(black_box(&plan), black_box(&table)).unwrap()));
    });

    c.bench_function("parse_plan", |b| {
        b.iter(|| {
            let s = parse_select(black_box(sql)).unwrap();
            black_box(plan_select(&s, TableStats { row_count: 100_000 }));
        });
    });
}

fn bench_vector_search(c: &mut Criterion) {
    let dim = 128;
    let embeddings: Vec<Vec<f32>> = (0..10_000)
        .map(|i| (0..dim).map(|d| ((i + d) % 7) as f32).collect())
        .collect();
    let query: Vec<f32> = (0..dim).map(|d| (d % 7) as f32).collect();

    c.bench_function("vector_topk_10k_dim128", |b| {
        b.iter(|| black_box(vector_topk(black_box(&embeddings), black_box(&query), 10)));
    });
}

criterion_group!(benches, bench_query, bench_vector_search);
criterion_main!(benches);
