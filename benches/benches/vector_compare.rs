//! Vector search comparison benchmarks.
//!
//! Compares `tpt-archon-relational`'s CPU `vector_topk` against:
//! - A pure brute-force baseline (measures Archon's overhead)
//! - pgvector via PostgreSQL (optional, `--features pgvector-bench` + POSTGRES_URL)
//! - SQLite via rusqlite (optional, `--features sqlite-bench` + SQLITE_PATH)
//!
//! Run with:
//!   cargo bench --bench vector_compare                          # Archon only
//!   cargo bench --bench vector_compare --features pgvector-bench  # + pgvector
//!   cargo bench --bench vector_compare --features sqlite-bench    # + SQLite

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use tpt_archon_relational::executor::vector_topk;

const DIM: usize = 128;
const DB_SIZES: &[usize] = &[1_000, 10_000, 100_000];
const K: usize = 10;

fn make_embeddings(n: usize) -> Vec<Vec<f32>> {
    (0..n)
        .map(|i| (0..DIM).map(|d| ((i + d) % 7) as f32).collect())
        .collect()
}

fn make_query() -> Vec<f32> {
    (0..DIM).map(|d| (d % 7) as f32).collect()
}

fn brute_force_topk(embeddings: &[Vec<f32>], query: &[f32], k: usize) -> Vec<usize> {
    let mut scored: Vec<(usize, f32)> = embeddings
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let dot: f32 = e.iter().zip(query).map(|(a, b)| a * b).sum();
            (i, dot)
        })
        .collect();
    scored.select_nth_unstable_by(k - 1, |a, b| b.1.partial_cmp(&a.1).unwrap());
    scored[..k].iter().map(|(i, _)| *i).collect()
}

fn bench_archon(c: &mut Criterion) {
    let query = make_query();
    let mut group = c.benchmark_group("vector_topk");
    for &n in DB_SIZES {
        let embeddings = make_embeddings(n);
        group.bench_with_input(BenchmarkId::new("archon", n), &embeddings, |b, emb| {
            b.iter(|| black_box(vector_topk(black_box(emb), black_box(&query), K)));
        });
        group.bench_with_input(
            BenchmarkId::new("brute_force", n),
            &embeddings,
            |b, emb| {
                b.iter(|| black_box(brute_force_topk(black_box(emb), black_box(&query), K)));
            },
        );
    }
    group.finish();
}

#[cfg(feature = "pgvector-bench")]
fn bench_pgvector(c: &mut Criterion) {
    let Ok(url) = std::env::var("POSTGRES_URL") else {
        eprintln!("POSTGRES_URL not set, skipping pgvector benchmarks");
        return;
    };

    let Ok(mut client) = postgres::Client::connect(&url, postgres::NoTls) else {
        eprintln!("Could not connect to PostgreSQL at {url}, skipping pgvector");
        return;
    };

    let _ = client.execute("CREATE EXTENSION IF NOT EXISTS vector", &[]);

    let query = make_query();
    let mut group = c.benchmark_group("pgvector_compare");

    for &n in DB_SIZES {
        let _ = client.execute("DROP TABLE IF EXISTS embeddings", &[]);
        client
            .execute(
                &format!("CREATE TABLE embeddings (id SERIAL PRIMARY KEY, emb vector({DIM}))"),
                &[],
            )
            .unwrap();

        let embeddings = make_embeddings(n);
        let mut tx = client.transaction().unwrap();
        for (i, e) in embeddings.iter().enumerate() {
            let vec_str = format!(
                "[{}]",
                e.iter()
                    .map(|f| f.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            );
            let id: i32 = i as i32;
            tx.execute(
                "INSERT INTO embeddings (id, emb) VALUES ($1, $2::vector)",
                &[&id, &vec_str],
            )
            .unwrap();
        }
        tx.commit().unwrap();

        let query_vec_str = format!(
            "[{}]",
            query
                .iter()
                .map(|f| f.to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
        let limit: i32 = K as i32;

        group.bench_with_input(BenchmarkId::new("pgvector_l2", n), &n, |b, _| {
            b.iter(|| {
                let _rows = client
                    .query(
                        "SELECT id FROM embeddings ORDER BY emb <=> $1::vector LIMIT $2",
                        &[&query_vec_str, &limit],
                    )
                    .unwrap();
            });
        });

        use tpt_archon_relational::executor::{Table, Value};
        let mut table = Table::new(vec!["id".to_string(), "emb".to_string()]);
        for (i, e) in embeddings.iter().enumerate() {
            table.insert(vec![Value::Int(i as i64), Value::Vector(e.clone())]);
        }
        group.bench_with_input(BenchmarkId::new("archon_cpu", n), &n, |b, _| {
            b.iter(|| {
                let emb_col: Vec<Vec<f32>> = table
                    .rows
                    .iter()
                    .filter_map(|r| match &r[1] {
                        Value::Vector(v) => Some(v.clone()),
                        _ => None,
                    })
                    .collect();
                black_box(vector_topk(black_box(&emb_col), black_box(&query), K));
            });
        });
    }
    group.finish();
}

#[cfg(not(feature = "pgvector-bench"))]
fn bench_pgvector(_c: &mut Criterion) {}

#[cfg(feature = "sqlite-bench")]
fn bench_sqlite(c: &mut Criterion) {
    let path = match std::env::var("SQLITE_PATH") {
        Ok(p) => p,
        Err(_) => {
            eprintln!("SQLITE_PATH not set, skipping SQLite benchmarks");
            return;
        }
    };

    let conn = rusqlite::Connection::open(&path).expect("open sqlite db");

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS embeddings (
            id INTEGER PRIMARY KEY,
            emb BLOB
        );",
    )
    .unwrap();

    let query = make_query();
    let mut group = c.benchmark_group("sqlite_compare");

    for &n in DB_SIZES {
        conn.execute_batch("DELETE FROM embeddings").unwrap();
        let embeddings = make_embeddings(n);
        for (i, e) in embeddings.iter().enumerate() {
            let blob: Vec<u8> = e.iter().flat_map(|f| f.to_le_bytes()).collect();
            conn.execute(
                "INSERT INTO embeddings (id, emb) VALUES (?1, ?2)",
                rusqlite::params![i as i64, blob],
            )
            .unwrap();
        }

        group.bench_with_input(BenchmarkId::new("sqlite_scan", n), &n, |b, _| {
            b.iter(|| {
                let mut stmt = conn.prepare("SELECT emb FROM embeddings").unwrap();
                let rows: Vec<Vec<f32>> = stmt
                    .query_map([], |row| {
                        let blob: Vec<u8> = row.get(0)?;
                        let floats: Vec<f32> = blob
                            .chunks_exact(4)
                            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                            .collect();
                        Ok(floats)
                    })
                    .unwrap()
                    .filter_map(|r| r.ok())
                    .collect();
                black_box(vector_topk(black_box(&rows), black_box(&query), K));
            });
        });
    }
    group.finish();
}

#[cfg(not(feature = "sqlite-bench"))]
fn bench_sqlite(_c: &mut Criterion) {}

criterion_group!(benches, bench_archon, bench_pgvector, bench_sqlite);
criterion_main!(benches);
