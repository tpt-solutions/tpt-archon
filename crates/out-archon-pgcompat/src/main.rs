//! Real-Postgres oracle for `tpt-archon-relational`'s `.slt` comparison
//! corpus (Phase 8 / Track C, "slice 1" of the PostgreSQL-compatibility
//! roadmap — see the plan doc and `TODO.md`).
//!
//! `tpt-archon-relational/tests/slt.rs` runs the same corpus against
//! `Database` directly and is a normal `cargo test --workspace` integration
//! test (zero new dependencies, always runs, never touches a network). This
//! binary is the other half: it runs `supported/*.slt` against a REAL
//! PostgreSQL server and asserts the corpus's expected rows are what
//! Postgres actually produces — which is what makes the corpus a validated
//! oracle rather than just a record of Archon's own behavior. It is
//! excluded from the root workspace (see root `Cargo.toml` and this crate's
//! own `Cargo.toml` comment) and is only ever run by the opt-in `pg-compat`
//! CI job or manually against a local `docker compose up postgres`.
//!
//! ## Corpus location
//!
//! Reads the *same* files `tests/slt.rs` reads — this binary does not
//! vendor or duplicate corpus content — at
//! `../tpt-archon-relational/tests/slt/` relative to `CARGO_MANIFEST_DIR`.
//!
//! ## Why a second small `.slt` parser
//!
//! This crate is excluded from the root workspace and cannot path-depend on
//! `tpt-archon-relational`'s test-only directive parser (that parser lives
//! in `tests/slt.rs`, which isn't part of that crate's library surface, and
//! this crate does not otherwise need to link `tpt-archon-relational` at
//! all — see this crate's `Cargo.toml` comment). Per the slice-1 spec, a
//! small amount of duplicated parsing logic between the two is acceptable;
//! both are kept deliberately minimal. See `tests/slt.rs`'s own doc comment
//! for the full directive-format description this parser also implements
//! (`statement ok`/`statement error <substring>`/`query <types>` + `----` +
//! expected rows); it is not repeated in full here.
//!
//! ## `divergent/` is skipped entirely
//!
//! `divergent/*.slt` files exist to document known ARCHON divergences from
//! Postgres — there is nothing to validate about Postgres there (by
//! definition, Postgres is not expected to match Archon's documented bug/gap
//! behavior). Re-running them against Postgres would require a second,
//! divergence-aware comparison mode (assert Postgres does something
//! DIFFERENT from the expected rows, or matches a *different* set of
//! expected rows/errors than the ones written for Archon) that the slice-1
//! spec explicitly allows deferring. Skipping is documented here as the
//! chosen (simpler) option for this pass.
//!
//! ## `statement error`: existence-of-error only, not pattern matching
//!
//! The corpus's `<pattern>` substrings for `statement error` (e.g.
//! `TableAlreadyExists`, `UnknownColumn`) are deliberately shaped to match
//! `DbError`'s `#[derive(Debug)]` output (see `tests/slt.rs`'s doc comment)
//! — they are Archon-internal identifiers, not portable Postgres error
//! text. Real Postgres reports the same underlying condition (duplicate
//! table, unknown column, etc.) with entirely different wording/SQLSTATEs.
//! So this oracle only asserts that Postgres ALSO produces an error for
//! `statement error` directives — it does not attempt to match the
//! substring pattern against Postgres's own error message.
//!
//! ## Type-agnostic row comparison via `row_to_json`
//!
//! Rather than introspecting each result column's Postgres OID and picking
//! a matching Rust type to fetch it as (int4 vs int8 vs numeric vs text...,
//! which the `postgres` crate requires to match exactly), every query is
//! wrapped as `SELECT row_to_json(t)::text FROM (<original query>) AS t`.
//! This always yields a single `text` column containing a flat JSON object
//! whose key order matches the underlying row's column order; a small
//! hand-rolled flat-JSON-value scanner (below) pulls out just the value
//! tokens (this corpus's queries never produce nested objects/arrays), in
//! order, formatted the same way `tests/slt.rs`'s `display_value` formats
//! Archon's own `Value` (NULL -> `NULL`, numbers -> bare decimal, text ->
//! unquoted). This sidesteps needing per-column-type wire decoding entirely.
//!
//! ## Per-file isolation
//!
//! Each `.slt` file gets a fresh, dedicated Postgres schema (dropped and
//! recreated before the file runs), matching `tests/slt.rs`'s "fresh
//! `Database::empty()` per file" rule, without needing to wrap the file in
//! an implicit outer transaction (which would break any file that issues
//! its own explicit `BEGIN`/`COMMIT`/`ROLLBACK`, e.g. `transactions.slt`).

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use postgres::Client;

enum Directive {
    StatementOk {
        sql: String,
        line: usize,
    },
    StatementError {
        sql: String,
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

/// Parses the directive subset described in this file's module doc comment
/// (and in full in `tpt-archon-relational/tests/slt.rs`'s doc comment) out
/// of raw `.slt` text. Deliberately does not retain `statement error`'s
/// pattern text — see the module doc comment on why this oracle only
/// checks that Postgres also errors, not the specific message.
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

        if trimmed.strip_prefix("statement error ").is_some() {
            let line = i + 1;
            i += 1;
            let sql = collect_block(&lines, &mut i, false);
            out.push(Directive::StatementError { sql, line });
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

/// Recursively collects `.slt` file paths under `dir`, sorted for
/// deterministic run order.
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

/// Extracts just the value tokens (in key order) from a flat JSON object
/// produced by Postgres's `row_to_json`, formatted to match
/// `tests/slt.rs`'s `display_value` convention (`NULL` literal, bare
/// decimal numbers, unquoted text). This corpus's queries only ever project
/// scalar int/text/null columns, so a full JSON parser isn't needed — this
/// scans `"key":value` pairs at the top level of a single-line `{...}`
/// object and pulls out each `value` term.
fn extract_json_row_values(json: &str) -> Vec<String> {
    let bytes = json.as_bytes();
    let mut i = 0usize;
    let mut values = Vec::new();

    // Skip to the first '{'.
    while i < bytes.len() && bytes[i] != b'{' {
        i += 1;
    }
    if i < bytes.len() {
        i += 1; // consume '{'
    }

    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] == b'}' {
            break;
        }
        // Skip the JSON key (a quoted string) and its trailing colon.
        if bytes[i] == b'"' {
            i += 1;
            while i < bytes.len() && bytes[i] != b'"' {
                if bytes[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1; // closing quote
        }
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b':') {
            i += 1;
        }
        // Now parse the value term.
        if i < bytes.len() && bytes[i] == b'"' {
            // A JSON string value.
            i += 1;
            let start = i;
            let mut unescaped = String::new();
            while i < bytes.len() && bytes[i] != b'"' {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    unescaped.push(bytes[i + 1] as char);
                    i += 2;
                } else {
                    unescaped.push(bytes[i] as char);
                    i += 1;
                }
            }
            let _ = start;
            values.push(unescaped);
            i += 1; // closing quote
        } else if json[i..].starts_with("null") {
            values.push("NULL".to_string());
            i += 4;
        } else {
            // A bare number (int or numeric).
            let start = i;
            while i < bytes.len()
                && (bytes[i].is_ascii_digit()
                    || matches!(bytes[i], b'-' | b'.' | b'e' | b'E' | b'+'))
            {
                i += 1;
            }
            values.push(json[start..i].to_string());
        }
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b',' {
            i += 1;
        }
    }

    values
}

/// Runs one `.slt` file's directives against `client`, inside its own
/// dedicated (dropped-and-recreated) schema. Appends a human-readable
/// message to `failures` for each directive that doesn't hold against real
/// Postgres; does not stop at the first failure, so one run reports every
/// mismatch in the file.
fn run_file(client: &mut Client, path: &Path, failures: &mut Vec<String>) {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            failures.push(format!("{}: failed to read file: {e}", path.display()));
            return;
        }
    };
    let directives = parse_directives(&text);

    // Fresh, dedicated schema per file — analogous to `tests/slt.rs`'s
    // fresh `Database::empty()` per file — without wrapping the file in an
    // implicit outer transaction (which would break any file using its own
    // explicit BEGIN/COMMIT/ROLLBACK).
    if let Err(e) = client.batch_execute(
        "DROP SCHEMA IF EXISTS pgcompat_slt CASCADE; \
         CREATE SCHEMA pgcompat_slt; \
         SET search_path TO pgcompat_slt, public;",
    ) {
        failures.push(format!(
            "{}: failed to reset pgcompat_slt schema: {e}",
            path.display()
        ));
        return;
    }

    for directive in directives {
        match directive {
            Directive::StatementOk { sql, line } => {
                if let Err(e) = client.execute(sql.as_str(), &[]) {
                    failures.push(format!(
                        "{}:{line}: `statement ok` failed against real Postgres: {e}\n    sql: {sql}",
                        path.display()
                    ));
                }
            }
            Directive::StatementError { sql, line } => {
                if client.execute(sql.as_str(), &[]).is_ok() {
                    failures.push(format!(
                        "{}:{line}: expected an error, but real Postgres accepted it\n    sql: {sql}",
                        path.display()
                    ));
                }
                // A failed statement aborts the current Postgres
                // transaction (if one is open); nothing in `supported/`
                // relies on later statements in the same file succeeding
                // inside the same still-open transaction after an error, so
                // no explicit ROLLBACK/recovery is needed here.
            }
            Directive::Query {
                sql,
                types,
                expected,
                ordered,
                line,
            } => {
                let wrapped = format!("SELECT row_to_json(t)::text AS j FROM ({sql}) AS t");
                let rows = match client.query(wrapped.as_str(), &[]) {
                    Ok(r) => r,
                    Err(e) => {
                        failures.push(format!(
                            "{}:{line}: `query` failed against real Postgres: {e}\n    sql: {sql}",
                            path.display()
                        ));
                        continue;
                    }
                };

                let mut actual: Vec<Vec<String>> = Vec::with_capacity(rows.len());
                let mut malformed = false;
                for row in &rows {
                    let json: Option<String> = row.get(0);
                    let json = json.unwrap_or_default();
                    let values = extract_json_row_values(&json);
                    if values.len() != types.len() {
                        failures.push(format!(
                            "{}:{line}: column count mismatch: type string {types:?} implies \
                             {} column(s), Postgres row_to_json gave {} ({json})\n    sql: {sql}",
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
                        "{}:{line}: row mismatch against real Postgres (order-sensitive: {ordered})\n    \
                         sql: {sql}\n    expected: {expected:?}\n    actual:   {actual:?}",
                        path.display()
                    ));
                }
            }
        }
    }
}

fn main() -> ExitCode {
    let url = match env::var("POSTGRES_URL") {
        Ok(u) => u,
        Err(_) => {
            eprintln!("POSTGRES_URL not set, skipping Postgres-oracle corpus validation");
            return ExitCode::SUCCESS;
        }
    };

    let mut client = match Client::connect(&url, postgres::NoTls) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Could not connect to PostgreSQL at {url}: {e}, skipping Postgres-oracle corpus validation");
            return ExitCode::SUCCESS;
        }
    };

    let corpus_root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../tpt-archon-relational/tests/slt");
    let supported_dir = corpus_root.join("supported");
    if !supported_dir.is_dir() {
        eprintln!(
            "corpus directory not found at {} — is this crate checked out alongside \
             tpt-archon-relational?",
            supported_dir.display()
        );
        return ExitCode::FAILURE;
    }

    // `divergent/` is intentionally skipped — see this file's module doc
    // comment ("`divergent/` is skipped entirely").
    let files = collect_slt_files(&supported_dir);
    if files.is_empty() {
        eprintln!("no .slt files found under {}", supported_dir.display());
        return ExitCode::FAILURE;
    }

    let mut failures = Vec::new();
    for path in &files {
        run_file(&mut client, path, &mut failures);
    }

    // Clean up the scratch schema so a local `docker compose up postgres`
    // instance doesn't accumulate one across repeated runs.
    let _ = client.batch_execute("DROP SCHEMA IF EXISTS pgcompat_slt CASCADE;");

    if failures.is_empty() {
        println!(
            "pg-compat: {} corpus file(s) validated against real Postgres, no mismatches",
            files.len()
        );
        ExitCode::SUCCESS
    } else {
        eprintln!(
            "pg-compat: {} failure(s) against real Postgres:",
            failures.len()
        );
        for f in &failures {
            eprintln!("  - {f}");
        }
        ExitCode::FAILURE
    }
}
