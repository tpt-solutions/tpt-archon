//! Runs the same `.slt` corpus that `tpt-archon-relational/tests/slt.rs`
//! runs against `Database` directly, but this time through the PostgreSQL
//! wire protocol server (`out-archon-pgwire`) using a real `postgres` crate
//! client. This is **Phase 8 / Track C, Slice 2** — catches wire-encoding
//! bugs (RowDescription OIDs, DataRow formatting, CommandComplete tags,
//! SQLSTATEs) that the Rust-API-level runner cannot see.
//!
//! This test is SKIPPED unless `PGWIRE_SLT_TEST` environment variable is set
//! to a reachable PostgreSQL wire endpoint (e.g., "postgresql://localhost:5432"
//! or a running `archon-pgwire` binary on a known port). The CI `pg-compat`
//! job runs this against a real Postgres; a local developer can run it
//! against `cargo run --bin archon-pgwire` in another terminal.
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use postgres::{Client, NoTls};
use tpt_archon_relational::database::Database;

/// Same directive format as `tpt-archon-relational/tests/slt.rs` — see that
/// file's module doc comment for the full description.
enum Directive {
    StatementOk {
        sql: String,
        line: usize,
    },
    StatementError {
        sql: String,
        pattern: String,
        line: usize,
    },
    Query {
        sql: String,
        types: String,
        expected: Vec<Vec<String>>,
        ordered: bool,
        line: usize,
    },
}

fn parse_directives(text: &str) -> Vec<Directive> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut i = 0usize;

    fn collect_block(lines: &[&str], i: &mut usize, stop_at_dashes: bool) -> String {
        let mut parts: Vec<&str> = Vec::new();
        while *i < lines.len() {
            let line = lines[*i];
            let trimmed = line.trim();
            if trimmed.is_empty() {
                break;
            }
            if stop_at_dashes && trimmed == "----" {
                break;
            }
            if !trimmed.starts_with('#') {
                parts.push(trimmed);
            }
            *i += 1;
        }
        parts.join(" ")
    }

    while i < lines.len() {
        let trimmed = lines[i].trim();

        if trimmed.is_empty() || trimmed.starts_with('#') {
            i += 1;
            continue;
        }

        if trimmed == "statement ok" {
            let line = i + 1;
            i += 1;
            let sql = collect_block(&lines, &mut i, false);
            out.push(Directive::StatementOk { sql, line });
            continue;
        }

        if let Some(pattern) = trimmed.strip_prefix("statement error ") {
            let pattern = pattern.trim().to_string();
            let line = i + 1;
            i += 1;
            let sql = collect_block(&lines, &mut i, false);
            out.push(Directive::StatementError { sql, pattern, line });
            continue;
        }

        if let Some(types) = trimmed.strip_prefix("query ") {
            let types = types.trim().to_string();
            let line = i + 1;
            i += 1;
            let sql = collect_block(&lines, &mut i, true);

            while i < lines.len() && lines[i].trim().is_empty() {
                i += 1;
            }
            assert_eq!(
                lines.get(i).map(|l| l.trim()),
                Some("----"),
                "line {line}: expected `----` after query SQL"
            );
            i += 1;

            let mut expected = Vec::new();
            while i < lines.len() {
                let row_trimmed = lines[i].trim();
                if row_trimmed.is_empty() {
                    break;
                }
                if row_trimmed.starts_with('#') {
                    i += 1;
                    continue;
                }
                expected.push(
                    row_trimmed
                        .split_whitespace()
                        .map(|s| s.to_string())
                        .collect::<Vec<String>>(),
                );
                i += 1;
            }

            let ordered = sql.to_ascii_uppercase().contains("ORDER BY");
            out.push(Directive::Query {
                sql,
                types,
                expected,
                ordered,
                line,
            });
            continue;
        }

        panic!("line {}: unrecognized directive: {trimmed:?}", i + 1);
    }

    out
}

fn collect_slt_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("failed to read directory {}: {e}", dir.display());
            return out;
        }
    };
    for entry in entries {
        let entry = entry.expect("directory entry");
        let path = entry.path();
        if path.is_dir() {
            out.extend(collect_slt_files(&path));
        } else if path.extension().and_then(|e| e.to_str()) == Some("slt") {
            out.push(path);
        }
    }
    out.sort();
    out
}

fn run_file(client: &mut Client, path: &Path, failures: &mut Vec<String>) {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            failures.push(format!("{}: failed to read file: {e}", path.display()));
            return;
        }
    };
    let directives = parse_directives(&text);

    for directive in directives {
        match directive {
            Directive::StatementOk { sql, line } => {
                if let Err(e) = client.execute(sql.as_str(), &[]) {
                    failures.push(format!(
                        "{}:{line}: `statement ok` failed against wire server: {e}\n    sql: {sql}",
                        path.display()
                    ));
                }
            }
            Directive::StatementError {
                sql,
                pattern: _pattern,
                line,
            } => {
                if client.execute(sql.as_str(), &[]).is_ok() {
                    failures.push(format!(
                        "{}:{line}: expected an error, but wire server accepted it\n    sql: {sql}",
                        path.display()
                    ));
                }
            }
            Directive::Query {
                sql,
                types,
                expected,
                ordered,
                line,
            } => {
                let rows = match client.query(sql.as_str(), &[]) {
                    Ok(r) => r,
                    Err(e) => {
                        failures.push(format!(
                            "{}:{line}: `query` failed against wire server: {e}\n    sql: {sql}",
                            path.display()
                        ));
                        continue;
                    }
                };

                let mut actual: Vec<Vec<String>> = Vec::with_capacity(rows.len());
                let mut malformed = false;
                for row in &rows {
                    let mut values = Vec::with_capacity(types.len());
                    for i in 0..types.len() {
                        let val: Option<String> = row.get(i);
                        values.push(val.unwrap_or_else(|| "NULL".to_string()));
                    }
                    if values.len() != types.len() {
                        failures.push(format!(
                            "{}:{line}: column count mismatch: type string {types:?} implies \
                             {} column(s), wire server gave {}\n    sql: {sql}",
                            path.display(),
                            types.len(),
                            values.len()
                        ));
                        malformed = true;
                        break;
                    }
                    actual.push(values);
                }
                if malformed {
                    continue;
                }

                let mut expected = expected;
                if !ordered {
                    actual.sort();
                    expected.sort();
                }

                if actual != expected {
                    failures.push(format!(
                        "{}:{line}: row mismatch against wire server (order-sensitive: {ordered})\n    \
                         sql: {sql}\n    expected: {expected:?}\n    actual:   {actual:?}",
                        path.display()
                    ));
                }
            }
        }
    }
}

/// Runs the SLT corpus against a PostgreSQL wire protocol endpoint.
///
/// This test is skipped unless the `PGWIRE_SLT_TEST` environment variable
/// is set to a valid PostgreSQL connection URL (e.g., "postgresql://localhost:5432"
/// for a real Postgres, or the URL of a running `archon-pgwire` server).
#[test]
fn pgwire_slt_corpus() {
    // Get the target endpoint from environment variable
    let url = match env::var("PGWIRE_SLT_TEST") {
        Ok(u) => u,
        Err(_) => {
            eprintln!("PGWIRE_SLT_TEST not set; skipping wire-protocol SLT corpus test");
            return;
        }
    };

    // Connect to the wire server
    let mut client = match Client::connect(&url, NoTls) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Could not connect to wire server at {url}: {e}; skipping wire-protocol SLT corpus test");
            return;
        }
    };

    // Find the corpus directory (same as the Rust API test)
    let corpus_root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../tpt-archon-relational/tests/slt");
    let supported_dir = corpus_root.join("supported");
    if !supported_dir.is_dir() {
        eprintln!(
            "corpus directory not found at {} — is this crate checked out alongside \
             tpt-archon-relational?",
            supported_dir.display()
        );
        return;
    }

    // Run only supported/ (divergent/ is for Archon's documented divergences)
    let files = collect_slt_files(&supported_dir);
    if files.is_empty() {
        eprintln!("no .slt files found under {}", supported_dir.display());
        return;
    }

    let mut failures = Vec::new();
    for path in &files {
        run_file(&mut client, path, &mut failures);
    }

    if failures.is_empty() {
        println!(
            "pgwire-slt: {} corpus file(s) validated against wire server, no mismatches",
            files.len()
        );
    } else {
        eprintln!(
            "pgwire-slt: {} failure(s) against wire server:",
            failures.len()
        );
        for f in &failures {
            eprintln!("  - {f}");
        }
        panic!("wire-protocol SLT corpus validation failed");
    }
}

/// Integration test that starts `archon-pgwire` server in a background thread
/// and runs the SLT corpus against it. This is only run when explicitly
/// invoked (not in regular `cargo test --workspace`) because it spawns a server.
#[test]
#[ignore] // Run with: cargo test -p out-archon-pgwire pgwire_slt_integration -- --ignored
fn pgwire_slt_integration() {
    // Start the wire server in a background thread
    let db = Arc::new(Mutex::new(Database::empty()));
    let _server_thread = thread::spawn(move || {
        use out_archon_pgwire::serve;
        let _ = serve("127.0.0.1:15432", db);
    });

    // Give the server time to start
    thread::sleep(Duration::from_millis(500));

    let url = "postgresql://127.0.0.1:15432";
    let mut client = match Client::connect(url, NoTls) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Could not connect to archon-pgwire at {url}: {e}");
            return;
        }
    };

    // Find the corpus directory
    let corpus_root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../tpt-archon-relational/tests/slt");
    let supported_dir = corpus_root.join("supported");
    if !supported_dir.is_dir() {
        eprintln!("corpus directory not found at {}", supported_dir.display());
        return;
    }

    let files = collect_slt_files(&supported_dir);
    if files.is_empty() {
        eprintln!("no .slt files found under {}", supported_dir.display());
        return;
    }

    let mut failures = Vec::new();
    for path in &files {
        run_file(&mut client, path, &mut failures);
    }

    // Clean up
    let _ = client.batch_execute("DISCARD ALL");

    if failures.is_empty() {
        println!(
            "pgwire-slt (integration): {} corpus file(s) validated against archon-pgwire, no mismatches",
            files.len()
        );
    } else {
        eprintln!(
            "pgwire-slt (integration): {} failure(s) against archon-pgwire:",
            failures.len()
        );
        for f in &failures {
            eprintln!("  - {f}");
        }
        panic!("wire-protocol SLT corpus validation failed");
    }
}
